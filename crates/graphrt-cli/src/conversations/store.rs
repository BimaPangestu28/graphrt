//! Where conversations are kept: in memory, or one JSON file each in the run
//! store directory (`<dir>/conversations/<id>.json`).
//!
//! The directory store follows the run store: files are replaced atomically
//! (temporary file, fsync, rename), created `0600` in a `0700` directory
//! because they hold what people wrote, and every operation holds an OS lock on
//! `<dir>/conversations/.lock`, so several servers may share the directory.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use graphrt_core::private_fs;

use super::Conversation;

/// Why a conversation store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    /// The id cannot name a conversation (it would not be a safe file name).
    #[error("invalid conversation id")]
    InvalidId,
    /// Names server paths: log it, answer generically.
    #[error("conversation store I/O error: {0}")]
    Io(String),
    #[error("corrupt conversation: {0}")]
    Corrupt(String),
}

pub type ConversationResult<T> = Result<T, ConversationError>;

/// A change applied to a stored conversation under the store's lock.
pub type Change = Box<dyn FnOnce(&mut Conversation) + Send>;

/// Storage of conversations. Every method is atomic with respect to the others.
pub trait ConversationStore: Send + Sync + std::fmt::Debug {
    /// Store a new conversation. With `keep_per_owner > 0` and an owner, the
    /// owner's least recently used conversations beyond that number are deleted.
    fn insert(&self, conversation: Conversation, keep_per_owner: usize) -> BoxFuture<'_, ConversationResult<()>>;

    /// The conversation `id`, if it exists.
    fn get(&self, id: &str) -> BoxFuture<'_, ConversationResult<Option<Conversation>>>;

    /// Apply `change` to the conversation `id` and return the result, or
    /// `None` when there is no such conversation.
    fn update(&self, id: &str, change: Change) -> BoxFuture<'_, ConversationResult<Option<Conversation>>>;

    /// Every conversation, the most recently used first.
    fn all(&self) -> BoxFuture<'_, ConversationResult<Vec<Conversation>>>;

    /// Delete the conversation `id`. Returns whether it existed.
    fn delete(&self, id: &str) -> BoxFuture<'_, ConversationResult<bool>>;

    /// Delete the conversations last used before `cutoff_ms`. Returns how many.
    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, ConversationResult<usize>>;
}

/// Ids are file names: letters, digits, `-` and `_`, at most 128 bytes.
fn validate_id(id: &str) -> ConversationResult<()> {
    let ok =
        !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { Ok(()) } else { Err(ConversationError::InvalidId) }
}

/// Most recently used first; ties by id so pages are stable.
fn sort_recent_first(conversations: &mut [Conversation]) {
    conversations.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.id.cmp(&a.id)));
}

/// The ids of `owner`'s conversations beyond the `keep` most recently used.
fn beyond_limit(mut owned: Vec<Conversation>, keep: usize) -> Vec<String> {
    sort_recent_first(&mut owned);
    owned.into_iter().skip(keep).map(|conversation| conversation.id).collect()
}

/// Conversations in memory: gone when the server stops.
#[derive(Debug, Default)]
pub struct MemoryConversations {
    conversations: Mutex<HashMap<String, Conversation>>,
}

impl MemoryConversations {
    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<String, Conversation>> {
        self.conversations.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ConversationStore for MemoryConversations {
    fn insert(&self, conversation: Conversation, keep_per_owner: usize) -> BoxFuture<'_, ConversationResult<()>> {
        Box::pin(async move {
            validate_id(&conversation.id)?;
            let mut all = self.locked();
            let owner = conversation.owner.clone();
            all.insert(conversation.id.clone(), conversation);
            if keep_per_owner > 0 && owner.is_some() {
                let owned = all.values().filter(|c| c.owner == owner).cloned().collect();
                for id in beyond_limit(owned, keep_per_owner) {
                    all.remove(&id);
                }
            }
            Ok(())
        })
    }

    fn get(&self, id: &str) -> BoxFuture<'_, ConversationResult<Option<Conversation>>> {
        let found = self.locked().get(id).cloned();
        Box::pin(async move { Ok(found) })
    }

    fn update(&self, id: &str, change: Change) -> BoxFuture<'_, ConversationResult<Option<Conversation>>> {
        let mut all = self.locked();
        let changed = all.get_mut(id).map(|conversation| {
            change(conversation);
            conversation.clone()
        });
        drop(all);
        Box::pin(async move { Ok(changed) })
    }

    fn all(&self) -> BoxFuture<'_, ConversationResult<Vec<Conversation>>> {
        let mut all: Vec<Conversation> = self.locked().values().cloned().collect();
        sort_recent_first(&mut all);
        Box::pin(async move { Ok(all) })
    }

    fn delete(&self, id: &str) -> BoxFuture<'_, ConversationResult<bool>> {
        let existed = self.locked().remove(id).is_some();
        Box::pin(async move { Ok(existed) })
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, ConversationResult<usize>> {
        let mut all = self.locked();
        let before = all.len();
        all.retain(|_, conversation| conversation.updated_ms >= cutoff_ms);
        let removed = before - all.len();
        drop(all);
        Box::pin(async move { Ok(removed) })
    }
}

#[derive(Debug)]
struct Dir {
    root: PathBuf,
    tmp_counter: AtomicU64,
}

/// Conversations as files in a directory: they survive restarts.
#[derive(Clone, Debug)]
pub struct FileConversations {
    dir: Arc<Dir>,
}

fn io(context: &str, e: &std::io::Error) -> ConversationError {
    ConversationError::Io(format!("{context}: {e}"))
}

struct LockGuard(fs::File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Closing the file releases the lock too; unlocking makes it prompt.
        let _released = self.0.unlock();
    }
}

impl FileConversations {
    /// Open (and create) `<store>/conversations`.
    ///
    /// @throws ConversationError - When the directory cannot be created.
    pub fn open(store: &Path) -> ConversationResult<Self> {
        let root = store.join("conversations");
        private_fs::create_dir_all(&root).map_err(|e| io("cannot create the conversations directory", &e))?;
        Ok(Self { dir: Arc::new(Dir { root, tmp_counter: AtomicU64::new(0) }) })
    }

    fn blocking<T, F>(&self, op: F) -> BoxFuture<'static, ConversationResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Dir) -> ConversationResult<T> + Send + 'static,
    {
        let dir = Arc::clone(&self.dir);
        Box::pin(async move {
            graphrt_core::task::spawn_blocking(move || {
                let _lock = dir.lock()?;
                op(&dir)
            })
            .await
            .map_err(|e| ConversationError::Io(format!("store task failed: {e}")))?
        })
    }
}

impl Dir {
    fn lock(&self) -> ConversationResult<LockGuard> {
        let path = self.root.join(".lock");
        // `LockFileEx` needs read or write access: an append-only handle is refused on Windows.
        #[cfg(windows)]
        let opened = private_fs::create_file(&path);
        #[cfg(not(windows))]
        let opened = private_fs::append_file(&path);
        let file = opened.map_err(|e| io("cannot open the conversations lock", &e))?;
        file.lock().map_err(|e| io("cannot lock the conversations directory", &e))?;
        Ok(LockGuard(file))
    }

    fn path(&self, id: &str) -> ConversationResult<PathBuf> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    fn read(&self, id: &str) -> ConversationResult<Option<Conversation>> {
        let path = self.path(id)?;
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ConversationError::Corrupt(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read a conversation", &e)),
        }
    }

    fn write(&self, conversation: &Conversation) -> ConversationResult<()> {
        let path = self.path(&conversation.id)?;
        let bytes = serde_json::to_vec(conversation).map_err(|e| ConversationError::Corrupt(e.to_string()))?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join(format!(".{}.{}.{n}.tmp", conversation.id, std::process::id()));
        let result = (|| {
            let mut file = private_fs::create_file(&tmp).map_err(|e| io("cannot create a temporary file", &e))?;
            file.write_all(&bytes).map_err(|e| io("cannot write a temporary file", &e))?;
            file.sync_all().map_err(|e| io("cannot sync a temporary file", &e))?;
            fs::rename(&tmp, &path).map_err(|e| io("cannot replace a conversation", &e))
        })();
        if result.is_err() {
            let _cleanup = fs::remove_file(&tmp);
        }
        result
    }

    fn remove(&self, id: &str) -> ConversationResult<bool> {
        match fs::remove_file(self.path(id)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io("cannot delete a conversation", &e)),
        }
    }

    /// Every stored conversation. A file that cannot be read as one is skipped
    /// (and reported), so one damaged file never hides the others.
    fn read_all(&self) -> ConversationResult<Vec<Conversation>> {
        let entries = fs::read_dir(&self.root).map_err(|e| io("cannot list conversations", &e))?;
        let mut all = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io("cannot list conversations", &e))?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else { continue };
            if id.starts_with('.') {
                continue;
            }
            match self.read(id) {
                Ok(Some(conversation)) => all.push(conversation),
                Ok(None) => {}
                Err(e) => eprintln!("warning: skipping conversation `{id}`: {e}"),
            }
        }
        sort_recent_first(&mut all);
        Ok(all)
    }
}

impl ConversationStore for FileConversations {
    fn insert(&self, conversation: Conversation, keep_per_owner: usize) -> BoxFuture<'_, ConversationResult<()>> {
        self.blocking(move |dir| {
            dir.write(&conversation)?;
            if keep_per_owner > 0 && conversation.owner.is_some() {
                let owned = dir.read_all()?.into_iter().filter(|c| c.owner == conversation.owner).collect();
                for id in beyond_limit(owned, keep_per_owner) {
                    dir.remove(&id)?;
                }
            }
            Ok(())
        })
    }

    fn get(&self, id: &str) -> BoxFuture<'_, ConversationResult<Option<Conversation>>> {
        let id = id.to_owned();
        self.blocking(move |dir| dir.read(&id))
    }

    fn update(&self, id: &str, change: Change) -> BoxFuture<'_, ConversationResult<Option<Conversation>>> {
        let id = id.to_owned();
        self.blocking(move |dir| {
            let Some(mut conversation) = dir.read(&id)? else { return Ok(None) };
            change(&mut conversation);
            dir.write(&conversation)?;
            Ok(Some(conversation))
        })
    }

    fn all(&self) -> BoxFuture<'_, ConversationResult<Vec<Conversation>>> {
        self.blocking(Dir::read_all)
    }

    fn delete(&self, id: &str) -> BoxFuture<'_, ConversationResult<bool>> {
        let id = id.to_owned();
        self.blocking(move |dir| dir.remove(&id))
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, ConversationResult<usize>> {
        self.blocking(move |dir| {
            let mut removed = 0;
            for conversation in dir.read_all()?.into_iter().filter(|c| c.updated_ms < cutoff_ms) {
                if dir.remove(&conversation.id)? {
                    removed += 1;
                }
            }
            Ok(removed)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{Message, Role};
    use super::*;

    fn conversation(id: &str, owner: Option<&str>, updated_ms: u64) -> Conversation {
        let mut conversation = Conversation::new(id, owner.map(str::to_owned), "chat", None, 1);
        conversation.updated_ms = updated_ms;
        conversation
    }

    /// The contract both stores keep.
    async fn behaves_like_a_store(store: &dyn ConversationStore) {
        store.insert(conversation("a", Some("u1"), 10), 0).await.unwrap();
        store.insert(conversation("b", Some("u2"), 20), 0).await.unwrap();
        assert_eq!(store.get("a").await.unwrap().unwrap().owner.as_deref(), Some("u1"));
        assert!(store.get("missing").await.unwrap().is_none());

        let changed = store
            .update(
                "a",
                Box::new(|c| {
                    c.messages.push(Message { role: Role::User, content: "hi".into(), run_id: None, created_ms: 30 });
                    c.updated_ms = 30;
                }),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.messages.len(), 1);
        assert_eq!(store.get("a").await.unwrap().unwrap().messages[0].content, "hi");
        assert!(store.update("missing", Box::new(|_| {})).await.unwrap().is_none());

        let ids: Vec<String> = store.all().await.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["a", "b"], "most recently used first");

        assert_eq!(store.purge_before(25).await.unwrap(), 1);
        assert!(store.get("b").await.unwrap().is_none());
        assert!(store.delete("a").await.unwrap());
        assert!(!store.delete("a").await.unwrap());
    }

    /// A new conversation beyond the owner's limit replaces their least recently used one.
    async fn keeps_the_most_recent_per_owner(store: &dyn ConversationStore) {
        store.insert(conversation("old", Some("u1"), 10), 2).await.unwrap();
        store.insert(conversation("mid", Some("u1"), 20), 2).await.unwrap();
        store.insert(conversation("other", Some("u2"), 5), 2).await.unwrap();
        store.insert(conversation("keyed", None, 1), 2).await.unwrap();
        store.insert(conversation("new", Some("u1"), 30), 2).await.unwrap();
        let mut ids: Vec<String> = store.all().await.unwrap().into_iter().map(|c| c.id).collect();
        ids.sort();
        assert_eq!(ids, ["keyed", "mid", "new", "other"]);
    }

    #[tokio::test]
    async fn the_memory_store_keeps_the_contract() {
        behaves_like_a_store(&MemoryConversations::default()).await;
        keeps_the_most_recent_per_owner(&MemoryConversations::default()).await;
    }

    #[tokio::test]
    async fn the_directory_store_keeps_the_contract_and_survives_a_reopen() {
        let dir = private_fs::create_fresh_temp_dir("graphrt-conversations").unwrap();
        behaves_like_a_store(&FileConversations::open(&dir).unwrap()).await;
        keeps_the_most_recent_per_owner(&FileConversations::open(&dir).unwrap()).await;
        let reopened = FileConversations::open(&dir).unwrap();
        assert_eq!(reopened.all().await.unwrap().len(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ids_that_are_not_file_names_are_refused() {
        let dir = private_fs::create_fresh_temp_dir("graphrt-conversations").unwrap();
        let store = FileConversations::open(&dir).unwrap();
        for id in ["../escape", "a/b", "", ".hidden", "a.json"] {
            assert!(matches!(store.get(id).await, Err(ConversationError::InvalidId)), "{id}");
            assert!(store.insert(conversation(id, None, 1), 0).await.is_err(), "{id}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_damaged_file_does_not_hide_the_others() {
        let dir = private_fs::create_fresh_temp_dir("graphrt-conversations").unwrap();
        let store = FileConversations::open(&dir).unwrap();
        store.insert(conversation("good", None, 1), 0).await.unwrap();
        fs::write(dir.join("conversations").join("bad.json"), b"{not json").unwrap();
        assert_eq!(store.all().await.unwrap().len(), 1);
        assert!(matches!(store.get("bad").await, Err(ConversationError::Corrupt(_))));
        let _ = fs::remove_dir_all(&dir);
    }
}
