//! A live project: the local replica, kept in sync with the server over a
//! WebSocket, reconnecting on its own.
//!
//! Edits go into the local document first ([`LiveProject::edit`]), so they
//! work offline: what changed while disconnected is sent in the handshake
//! of the next connection, and merges with what others did meanwhile.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::{ClientError, ClientResult};
use crate::ProjectDoc;
use crate::protocol::{Control, Frame, Person, Presence, SUBPROTOCOL};

/// Gives a fresh workspace scoped access token (from a refresh token, usually).
pub type TokenSource = Arc<dyn Fn() -> BoxFuture<'static, ClientResult<String>> + Send + Sync>;

/// Someone else in the project.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub client_id: u64,
    /// Who they are, as the server says.
    pub person: Option<Person>,
    /// Their presence as they set it (focus, selection...), `user` removed.
    pub state: Value,
}

/// What happens in a live project.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveEvent {
    /// Connected (again): who this client is and whether it may edit.
    Connected { you: Person, read_only: bool },
    /// The first state of the server arrived on this connection: the project as
    /// both sides now have it. Apply local changes after this, so they are edits
    /// of the shared state, not of an empty one.
    Synced(Value),
    /// The project changed by someone else: its whole JSON.
    Changed(Value),
    /// Who else is there.
    Presence(Vec<Peer>),
    /// A change of this client was not applied, and why.
    Refused(String),
    /// This client's role changed.
    Role { role: String, read_only: bool },
    /// A version was saved.
    VersionSaved { version: u64, label: String, by: String },
    /// Disconnected. It reconnects unless the reason is final (`final_: true`).
    Disconnected { reason: String, final_: bool },
}

enum Command {
    Edit(serde_json::Map<String, Value>, Option<tokio::sync::oneshot::Sender<ClientResult<bool>>>),
    Presence(Value),
    SaveVersion(String),
    Close,
}

/// A handle on a live project.
#[derive(Debug, Clone)]
pub struct LiveProject {
    doc: ProjectDoc,
    commands: mpsc::UnboundedSender<Command>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Edit(..) => "Edit",
            Command::Presence(_) => "Presence",
            Command::SaveVersion(_) => "SaveVersion",
            Command::Close => "Close",
        })
    }
}

/// How the live project connects.
#[derive(Clone)]
pub struct LiveOptions {
    /// `wss://.../sync` ([`super::Api::sync_url`]).
    pub url: String,
    pub token: TokenSource,
    /// How often the token is renewed during a session (before it expires).
    pub renew_every: Duration,
    /// The longest wait between reconnection attempts.
    pub max_backoff: Duration,
}

impl std::fmt::Debug for LiveOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOptions").field("url", &self.url).finish_non_exhaustive()
    }
}

impl LiveProject {
    /// Start syncing `doc` (a fresh one, or one restored from [`Self::state`]).
    ///
    /// @returns The handle and the events.
    pub fn start(doc: ProjectDoc, options: LiveOptions) -> (Self, mpsc::UnboundedReceiver<LiveEvent>) {
        super::install_tls_provider();
        let (commands, rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(doc.clone(), options, rx, events));
        (Self { doc, commands }, events_rx)
    }

    /// The local replica.
    pub fn doc(&self) -> &ProjectDoc {
        &self.doc
    }

    /// The project as JSON, as this replica has it.
    pub fn json(&self) -> Value {
        self.doc.to_json()
    }

    /// The local replica encoded, to keep it while the app is closed (offline edits included).
    pub fn state(&self) -> Vec<u8> {
        self.doc.encode_state()
    }

    /// Make the project equal `project`: applied locally at once, sent when connected.
    ///
    /// @returns Whether anything changed.
    /// @throws ClientError - When the content is refused locally (too deep) or the session ended.
    pub async fn edit(&self, project: serde_json::Map<String, Value>) -> ClientResult<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands.send(Command::Edit(project, Some(tx))).map_err(|_| ClientError::Closed)?;
        rx.await.map_err(|_| ClientError::Closed)?
    }

    /// Announce this client's presence (what it looks at), e.g. `{"focus": "node_id"}`.
    pub fn set_presence(&self, state: Value) {
        let _closed = self.commands.send(Command::Presence(state));
    }

    /// Ask the server to keep the current state as a named version.
    pub fn save_version(&self, label: &str) {
        let _closed = self.commands.send(Command::SaveVersion(label.to_owned()));
    }

    /// Stop syncing (the local replica stays usable).
    pub fn close(&self) {
        let _closed = self.commands.send(Command::Close);
    }
}

/// Reasons the server gives that reconnecting will not fix.
fn is_final(reason: &str) -> bool {
    reason.contains("no longer a member") || reason.contains("deleted") || reason.contains("disabled")
}

async fn run(
    doc: ProjectDoc,
    options: LiveOptions,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<LiveEvent>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut clock: u32 = 0;
    let mut presence: Option<Value> = None;
    loop {
        let outcome = session(&doc, &options, &mut commands, &events, &mut clock, &mut presence).await;
        let (reason, closed_by_us) = match outcome {
            Ended::ByUs => return,
            Ended::Final(reason) => {
                let _gone = events.send(LiveEvent::Disconnected { reason, final_: true });
                return;
            }
            Ended::Reason(reason, connected) => {
                if connected {
                    backoff = Duration::from_millis(500);
                }
                (reason, false)
            }
        };
        let final_ = is_final(&reason);
        let _gone = events.send(LiveEvent::Disconnected { reason, final_ });
        if final_ || closed_by_us {
            return;
        }
        // Wait before reconnecting, still applying local edits (offline).
        let wait = tokio::time::sleep(backoff);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                () = &mut wait => break,
                command = commands.recv() => match command {
                    None | Some(Command::Close) => return,
                    Some(Command::Edit(project, reply)) => {
                        let result = doc.apply_json(&project).map(|u| u.is_some()).map_err(ClientError::from);
                        if let Some(reply) = reply { let _ = reply.send(result); }
                    }
                    Some(Command::Presence(state)) => presence = Some(state),
                    Some(Command::SaveVersion(_)) => {}
                },
            }
        }
        backoff = (backoff * 2).min(options.max_backoff);
    }
}

enum Ended {
    ByUs,
    /// Why, when reconnecting cannot help.
    Final(String),
    /// Why, and whether the session got as far as connected.
    Reason(String, bool),
}

async fn session(
    doc: &ProjectDoc,
    options: &LiveOptions,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &mpsc::UnboundedSender<LiveEvent>,
    clock: &mut u32,
    presence: &mut Option<Value>,
) -> Ended {
    let token = match (options.token)().await {
        Ok(token) => token,
        Err(error) => return Ended::Reason(format!("cannot get an access token: {error}"), false),
    };
    let socket = match connect(&options.url, &token).await {
        Ok(socket) => socket,
        // Refused for who this is (not a member, no such project, a token that does
        // not fit): trying again will not help.
        Err(error @ ClientError::Http { status: 401 | 403 | 404, .. }) => return Ended::Final(error.to_string()),
        Err(error) => return Ended::Reason(error.to_string(), false),
    };
    let (mut sink, mut stream) = socket.split();
    let send = |frame: Frame| Message::Binary(frame.encode().into());
    // Our state vector: the server answers with what we lack.
    if sink.send(send(Frame::SyncStep1(doc.state_vector()))).await.is_err() {
        return Ended::Reason("the connection closed".into(), false);
    }
    let my_id = doc.client_id();
    let mut peers: HashMap<u64, Peer> = HashMap::new();
    let mut connected = false;
    let mut synced = false;
    let mut renew = tokio::time::interval(options.renew_every);
    renew.tick().await;
    let presence_frame = |clock: u32, state: &Value| {
        Frame::Awareness(vec![Presence { client_id: my_id, clock, state: state.to_string() }])
    };
    if let Some(state) = presence.as_ref() {
        *clock += 1;
        let _ = sink.send(send(presence_frame(*clock, state))).await;
    }
    loop {
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(Message::Binary(bytes))) => bytes,
                    Some(Ok(Message::Close(frame))) => {
                        let reason = frame.map(|f| f.reason.to_string()).unwrap_or_else(|| "the server closed the session".into());
                        return Ended::Reason(reason, connected);
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => return Ended::Reason(error.to_string(), connected),
                    None => return Ended::Reason("the connection closed".into(), connected),
                };
                let Ok(frame) = Frame::decode(&bytes) else { continue };
                let is_step2 = matches!(frame, Frame::SyncStep2(_));
                match frame {
                    Frame::SyncStep1(sv) => {
                        if let Ok(update) = doc.diff(&sv)
                            && sink.send(send(Frame::SyncStep2(update))).await.is_err()
                        {
                            return Ended::Reason("the connection closed".into(), connected);
                        }
                    }
                    Frame::SyncStep2(update) | Frame::Update(update) => {
                        let first_state = is_step2 && !synced;
                        let before = doc.to_json();
                        if doc.apply_update(&update).is_ok() {
                            let after = doc.to_json();
                            if after != before {
                                let _ = events.send(LiveEvent::Changed(after.clone()));
                            }
                            if first_state {
                                synced = true;
                                let _ = events.send(LiveEvent::Synced(after));
                            }
                        }
                    }
                    Frame::Awareness(entries) => {
                        for entry in entries {
                            if entry.client_id == my_id {
                                continue;
                            }
                            if entry.state == "null" {
                                peers.remove(&entry.client_id);
                                continue;
                            }
                            let mut state: Value = serde_json::from_str(&entry.state).unwrap_or(Value::Null);
                            let person = state.as_object_mut().and_then(|o| o.remove("user")).and_then(|u| serde_json::from_value(u).ok());
                            peers.insert(entry.client_id, Peer { client_id: entry.client_id, person, state });
                        }
                        let mut list: Vec<Peer> = peers.values().cloned().collect();
                        list.sort_by_key(|p| p.client_id);
                        let _ = events.send(LiveEvent::Presence(list));
                    }
                    Frame::Control(Control::Welcome { you, read_only, .. }) => {
                        connected = true;
                        let _ = events.send(LiveEvent::Connected { you, read_only });
                    }
                    Frame::Control(Control::Refused { reason }) => { let _ = events.send(LiveEvent::Refused(reason)); }
                    Frame::Control(Control::Role { role, read_only }) => { let _ = events.send(LiveEvent::Role { role, read_only }); }
                    Frame::Control(Control::VersionSaved { version, label, by }) => {
                        let _ = events.send(LiveEvent::VersionSaved { version, label, by });
                    }
                    Frame::Control(Control::Closing { reason }) => return Ended::Reason(reason, connected),
                    Frame::Control(_) | Frame::AwarenessQuery | Frame::Auth(_) => {}
                }
            }
            command = commands.recv() => match command {
                None | Some(Command::Close) => {
                    *clock += 1;
                    let gone = Frame::Awareness(vec![Presence { client_id: my_id, clock: *clock, state: "null".into() }]);
                    let _ = sink.send(send(gone)).await;
                    let _ = sink.send(Message::Close(None)).await;
                    return Ended::ByUs;
                }
                Some(Command::Edit(project, reply)) => {
                    let result = doc.apply_json(&project).map_err(ClientError::from);
                    let changed = matches!(result, Ok(Some(_)));
                    if let Ok(Some(update)) = &result
                        && sink.send(send(Frame::Update(update.clone()))).await.is_err()
                    {
                        // Kept locally: sent in the next handshake.
                        if let Some(reply) = reply { let _ = reply.send(Ok(true)); }
                        return Ended::Reason("the connection closed".into(), connected);
                    }
                    if let Some(reply) = reply { let _ = reply.send(result.map(|_| changed)); }
                }
                Some(Command::Presence(state)) => {
                    *clock += 1;
                    let _ = sink.send(send(presence_frame(*clock, &state))).await;
                    *presence = Some(state);
                }
                Some(Command::SaveVersion(label)) => {
                    let _ = sink.send(send(Frame::Control(Control::SaveVersion { label }))).await;
                }
            },
            _ = renew.tick() => {
                match (options.token)().await {
                    Ok(token) => {
                        if sink.send(send(Frame::Control(Control::Token { token }))).await.is_err() {
                            return Ended::Reason("the connection closed".into(), connected);
                        }
                    }
                    Err(error) => return Ended::Reason(format!("cannot renew the access token: {error}"), connected),
                }
            }
        }
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str, token: &str) -> ClientResult<Socket> {
    let mut request = url.into_client_request().map_err(|error| ClientError::Address(error.to_string()))?;
    let headers = request.headers_mut();
    let bearer = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| ClientError::Internal(e.to_string()))?;
    headers.insert("authorization", bearer);
    headers.insert("sec-websocket-protocol", HeaderValue::from_static(SUBPROTOCOL));
    let connector = if url.starts_with("wss://") {
        use rustls_platform_verifier::ConfigVerifierExt as _;
        let config = rustls::ClientConfig::with_platform_verifier().map_err(|e| ClientError::Tls(e.to_string()))?;
        Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024));
    let (socket, _response) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async_tls_with_config(request, Some(config), true, connector),
    )
    .await
    .map_err(|_| ClientError::Unreachable("the server did not answer in time".into()))?
    .map_err(|error| match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let message = response
                .body()
                .as_ref()
                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            ClientError::Http { status, message }
        }
        tokio_tungstenite::tungstenite::Error::Tls(tls) => ClientError::Tls(tls.to_string()),
        other => ClientError::Unreachable(other.to_string()),
    })?;
    Ok(socket)
}
