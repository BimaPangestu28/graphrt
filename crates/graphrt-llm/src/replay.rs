//! Record and replay model responses ("cassettes") for deterministic runs.
//!
//! A [`ReplayModel`] sits where a provider would. Each request is normalized
//! (object keys sorted) and hashed with SHA-256; the hash names a cassette file
//! `<dir>/<key>.json` holding the request and the streamed response deltas.
//!
//! | Mode | Cassette present | Cassette missing |
//! |---|---|---|
//! | [`ReplayMode::Replay`] | served, chunk by chunk | error naming the key |
//! | [`ReplayMode::Record`] | re-recorded from the upstream | recorded from the upstream |
//! | [`ReplayMode::Auto`] | served | recorded from the upstream |
//!
//! Only successful, complete responses are written; a failed or interrupted
//! upstream stream leaves no cassette behind. Files are written atomically
//! (temp file + rename), so concurrent recordings of the same request are safe.
//!
//! Providers files declare it with `kind: replay` (see [`crate::providers`]);
//! `GRAPHRT_REPLAY_MODE` overrides the mode of every replay provider.
//!
//! Cassettes contain the full prompts and answers: treat them like test
//! fixtures, and do not record prompts that carry secrets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::LlmError;
use crate::types::{ChatDelta, ChatModel, ChatRequest, ChatStream, FinishReason};

/// Environment variable overriding the mode of every replay provider.
pub const REPLAY_MODE_ENV: &str = "GRAPHRT_REPLAY_MODE";

/// Cassette file format version.
pub const CASSETTE_VERSION: u32 = 1;

/// How a [`ReplayModel`] treats requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayMode {
    /// Serve recorded responses only; a request without a cassette is an error.
    #[default]
    Replay,
    /// Always call the upstream provider and (re)write the cassette.
    Record,
    /// Serve a recorded response when there is one, otherwise record it.
    Auto,
}

impl ReplayMode {
    /// Parse `replay`, `record` or `auto` (case-insensitive).
    pub fn parse(text: &str) -> Result<Self, LlmError> {
        match text.trim().to_ascii_lowercase().as_str() {
            "replay" => Ok(Self::Replay),
            "record" => Ok(Self::Record),
            "auto" => Ok(Self::Auto),
            other => {
                Err(LlmError::Config(format!("unknown replay mode `{other}` (expected `replay`, `record` or `auto`)")))
            }
        }
    }

    /// Whether this mode may call the upstream provider.
    pub fn needs_upstream(self) -> bool {
        self != Self::Replay
    }
}

/// One recorded delta (the serializable mirror of [`ChatDelta`]).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CassetteDelta {
    Text(String),
    Reasoning(String),
    ToolCall {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments: String,
    },
    Usage(Usage),
    Finish(FinishReason),
    ProviderState(serde_json::Value),
    Warning(String),
}

impl CassetteDelta {
    fn from_delta(delta: &ChatDelta) -> Self {
        match delta {
            ChatDelta::Text(t) => Self::Text(t.clone()),
            ChatDelta::Reasoning(t) => Self::Reasoning(t.clone()),
            ChatDelta::ToolCall { index, id, name, arguments } => {
                Self::ToolCall { index: *index, id: id.clone(), name: name.clone(), arguments: arguments.clone() }
            }
            ChatDelta::Usage(u) => Self::Usage(*u),
            ChatDelta::Finish(f) => Self::Finish(f.clone()),
            ChatDelta::ProviderState(s) => Self::ProviderState(s.clone()),
            ChatDelta::Warning(w) => Self::Warning(w.clone()),
        }
    }

    fn into_delta(self) -> ChatDelta {
        match self {
            Self::Text(t) => ChatDelta::Text(t),
            Self::Reasoning(t) => ChatDelta::Reasoning(t),
            Self::ToolCall { index, id, name, arguments } => ChatDelta::ToolCall { index, id, name, arguments },
            Self::Usage(u) => ChatDelta::Usage(u),
            Self::Finish(f) => ChatDelta::Finish(f),
            Self::ProviderState(s) => ChatDelta::ProviderState(s),
            Self::Warning(w) => ChatDelta::Warning(w),
        }
    }
}

/// A recorded request and its streamed response.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Cassette {
    pub version: u32,
    /// The cassette key ([`cassette_key`] of `request`).
    pub key: String,
    /// The normalized request, for humans reviewing the fixture.
    pub request: serde_json::Value,
    /// Response deltas in the order they were streamed.
    pub response: Vec<CassetteDelta>,
}

/// A request in canonical form: the serialized [`ChatRequest`] with every
/// object's keys sorted, so key order in workflow or schema documents does
/// not change the key.
pub fn normalize_request(request: &ChatRequest) -> Result<serde_json::Value, LlmError> {
    let value = serde_json::to_value(request).map_err(|e| LlmError::Config(format!("encoding request: {e}")))?;
    Ok(sort_keys(value))
}

fn sort_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Stable cassette key of a request: the first 32 hex digits of the SHA-256
/// of its normalized JSON.
pub fn cassette_key(request: &ChatRequest) -> Result<String, LlmError> {
    Ok(key_of(&normalize_request(request)?))
}

fn key_of(normalized: &serde_json::Value) -> String {
    let digest = Sha256::digest(normalized.to_string().as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// A model that serves (and optionally records) responses from cassette files.
#[derive(Debug)]
pub struct ReplayModel {
    dir: PathBuf,
    mode: ReplayMode,
    upstream: Option<Arc<dyn ChatModel>>,
}

impl ReplayModel {
    /// Serve cassettes from `dir`. Modes other than [`ReplayMode::Replay`]
    /// need an `upstream` model to record from.
    pub fn new(
        dir: impl Into<PathBuf>,
        mode: ReplayMode,
        upstream: Option<Arc<dyn ChatModel>>,
    ) -> Result<Self, LlmError> {
        if mode.needs_upstream() && upstream.is_none() {
            return Err(LlmError::Config(format!(
                "replay mode `{}` needs an `upstream` provider to record from",
                mode_name(mode)
            )));
        }
        Ok(Self { dir: dir.into(), mode, upstream })
    }

    /// The cassette directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The active mode.
    pub fn mode(&self) -> ReplayMode {
        self.mode
    }

    fn cassette_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    async fn serve(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let normalized = normalize_request(&request)?;
        let key = key_of(&normalized);
        let path = self.cassette_path(&key);
        if self.mode != ReplayMode::Record && path.is_file() {
            return replay_file(&path);
        }
        let Some(upstream) = self.upstream.as_ref().filter(|_| self.mode.needs_upstream()) else {
            return Err(LlmError::Config(format!(
                "replay: no cassette for this request (model `{}`, key {key}) in `{}`; record it with \
                 {REPLAY_MODE_ENV}=record or `mode: auto` and an `upstream` provider",
                request.model,
                self.dir.display()
            )));
        };
        let inner = upstream.chat(request).await?;
        Ok(record_stream(inner, Recording { path, key, request: normalized }))
    }
}

fn mode_name(mode: ReplayMode) -> &'static str {
    match mode {
        ReplayMode::Replay => "replay",
        ReplayMode::Record => "record",
        ReplayMode::Auto => "auto",
    }
}

impl ChatModel for ReplayModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(self.serve(request))
    }
}

/// Read a cassette and stream its deltas one by one.
fn replay_file(path: &Path) -> Result<ChatStream, LlmError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| LlmError::Config(format!("replay: cannot read cassette `{}`: {e}", path.display())))?;
    let cassette: Cassette = serde_json::from_str(&text)
        .map_err(|e| LlmError::Config(format!("replay: invalid cassette `{}`: {e}", path.display())))?;
    if cassette.version != CASSETTE_VERSION {
        return Err(LlmError::Config(format!(
            "replay: cassette `{}` has version {}, expected {CASSETTE_VERSION}",
            path.display(),
            cassette.version
        )));
    }
    let deltas = cassette.response.into_iter().map(|d| Ok(d.into_delta()));
    Ok(Box::pin(futures::stream::iter(deltas)))
}

struct Recording {
    path: PathBuf,
    key: String,
    request: serde_json::Value,
}

struct RecordState {
    inner: ChatStream,
    recording: Option<Recording>,
    deltas: Vec<CassetteDelta>,
}

/// Pass the upstream deltas through unchanged while collecting them; write the
/// cassette when the stream ends cleanly. A write failure surfaces as a final
/// error item so it is never silent.
fn record_stream(inner: ChatStream, recording: Recording) -> ChatStream {
    let state = RecordState { inner, recording: Some(recording), deltas: Vec::new() };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        match state.inner.next().await {
            Some(Ok(delta)) => {
                state.deltas.push(CassetteDelta::from_delta(&delta));
                Some((Ok(delta), state))
            }
            Some(Err(error)) => {
                state.recording = None;
                Some((Err(error), state))
            }
            None => {
                let recording = state.recording.take()?;
                let deltas = std::mem::take(&mut state.deltas);
                match write_cassette(recording, deltas) {
                    Ok(()) => None,
                    Err(error) => Some((Err(error), state)),
                }
            }
        }
    }))
}

fn write_cassette(recording: Recording, response: Vec<CassetteDelta>) -> Result<(), LlmError> {
    let Recording { path, key, request } = recording;
    let cassette = Cassette { version: CASSETTE_VERSION, key, request, response };
    let io = |e: std::io::Error| LlmError::Config(format!("replay: cannot write cassette `{}`: {e}", path.display()));
    // Cassettes hold prompts and answers: owner-only, like every run artifact.
    if let Some(dir) = path.parent() {
        graphrt_core::private_fs::create_dir_all(dir).map_err(io)?;
    }
    let mut text = serde_json::to_string_pretty(&cassette).map_err(|e| LlmError::Config(e.to_string()))?;
    text.push('\n');
    let temp = path.with_extension(format!("json.{}.{:?}.tmp", std::process::id(), std::thread::current().id()));
    graphrt_core::private_fs::write(&temp, text).map_err(io)?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _cleanup = std::fs::remove_file(&temp);
        io(e)
    })
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::mock::MockModel;
    use crate::types::{Message, collect};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-replay-{name}-{}", std::process::id()));
        let _fresh = std::fs::remove_dir_all(&dir);
        dir
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
    }

    async fn ask(model: &dyn ChatModel, text: &str) -> Result<crate::ChatResponse, LlmError> {
        let stream = model.chat(ChatRequest::new("m", vec![Message::user(text)])).await?;
        collect(stream, |_| {}).await
    }

    #[test]
    fn records_then_replays_chunk_by_chunk_and_misses_clearly() {
        let dir = scratch("cycle");
        let upstream = Arc::new(MockModel::new(vec![vec![
            ChatDelta::Text("Hel".into()),
            ChatDelta::Text("lo".into()),
            ChatDelta::Usage(Usage { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 }),
            ChatDelta::Finish(FinishReason::Stop),
        ]]));
        let recorder = ReplayModel::new(&dir, ReplayMode::Record, Some(upstream.clone())).unwrap();
        let recorded = block_on(ask(&recorder, "hi")).unwrap();
        assert_eq!(recorded.text, "Hello");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let player = ReplayModel::new(&dir, ReplayMode::Replay, None).unwrap();
        let stream = block_on(player.chat(ChatRequest::new("m", vec![Message::user("hi")]))).unwrap();
        let deltas: Vec<ChatDelta> = block_on(stream.map(|d| d.unwrap()).collect());
        assert_eq!(deltas.len(), 4, "chunks are replayed one by one");
        assert_eq!(deltas.first(), Some(&ChatDelta::Text("Hel".into())));
        let replayed = block_on(ask(&player, "hi")).unwrap();
        assert_eq!(replayed.text, "Hello");
        assert_eq!(replayed.usage.total_tokens, 5);
        assert_eq!(upstream.requests().len(), 1, "replay never calls the upstream");

        let miss = block_on(ask(&player, "something else")).unwrap_err().to_string();
        assert!(miss.contains("no cassette") && miss.contains("GRAPHRT_REPLAY_MODE"), "{miss}");
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_records_misses_and_replays_hits() {
        let dir = scratch("auto");
        let upstream = Arc::new(MockModel::new(vec![MockModel::text("one"), MockModel::text("two")]));
        let auto = ReplayModel::new(&dir, ReplayMode::Auto, Some(upstream.clone())).unwrap();
        assert_eq!(block_on(ask(&auto, "a")).unwrap().text, "one");
        assert_eq!(block_on(ask(&auto, "a")).unwrap().text, "one", "second call is served from the cassette");
        assert_eq!(block_on(ask(&auto, "b")).unwrap().text, "two");
        assert_eq!(upstream.requests().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700, "cassette directory");
            for entry in std::fs::read_dir(&dir).unwrap() {
                assert_eq!(mode(&entry.unwrap().path()), 0o600, "cassettes hold prompts and answers");
            }
        }
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_upstream_streams_are_not_recorded() {
        let dir = scratch("fail");
        let upstream = Arc::new(MockModel::new(vec![]));
        upstream.push_error(LlmError::Network("down".into()));
        let auto = ReplayModel::new(&dir, ReplayMode::Auto, Some(upstream)).unwrap();
        assert!(block_on(ask(&auto, "a")).is_err());
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().count() == 0);
    }

    #[test]
    fn keys_ignore_object_key_order_and_modes_need_upstreams() {
        let mut a = ChatRequest::new("m", vec![Message::user("x")]);
        a.extra.insert("b".into(), serde_json::json!(1));
        a.extra.insert("a".into(), serde_json::json!({"y": 1, "x": 2}));
        let mut b = ChatRequest::new("m", vec![Message::user("x")]);
        b.extra.insert("a".into(), serde_json::json!({"x": 2, "y": 1}));
        b.extra.insert("b".into(), serde_json::json!(1));
        assert_eq!(cassette_key(&a).unwrap(), cassette_key(&b).unwrap());
        assert_eq!(cassette_key(&a).unwrap().len(), 32);
        assert_ne!(cassette_key(&a).unwrap(), cassette_key(&ChatRequest::new("other", vec![])).unwrap());

        assert!(ReplayModel::new("x", ReplayMode::Auto, None).is_err());
        assert_eq!(ReplayMode::parse("RECORD").unwrap(), ReplayMode::Record);
        assert!(ReplayMode::parse("sometimes").is_err());
    }
}
