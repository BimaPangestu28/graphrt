//! One sync session: a WebSocket between an app and the room of a project.
//!
//! The HTTP handler checked the token, the membership and the project before
//! the upgrade ([`super::http`]). Here every message is checked again as it
//! comes: its size (the WebSocket limit), the session's message rate, the
//! member's current role (a viewer's edits are refused, a role change applies
//! at once), the token's expiry (the app sends a new one before it runs out).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use graphrt_console::Role;
use graphrt_console::audit::{Actor, Event};
use tokio::sync::mpsc;

use super::Server;
use super::accounts::Account;
use super::error::ServerError;
use super::rate::{Bucket, Rule};
use super::room::{Outgoing, PEER_QUEUE, Room, role_frame};
use super::util::now_ms;
use crate::protocol::{Control, Frame, Person, color_of};

/// Refused messages a session may send before it is closed.
const MAX_STRIKES: u32 = 20;

/// Everything a session needs, checked before the upgrade.
#[derive(Debug)]
pub(crate) struct Session {
    pub account: Account,
    pub role: Role,
    pub workspace: String,
    pub project: String,
    /// When the access token expires (Unix seconds).
    pub expires_secs: u64,
    pub room: Arc<Room>,
}

fn actor(account: &Account, role: Role) -> Actor {
    Actor { kind: "user".into(), id: account.id.clone(), email: account.email.clone(), role: Some(role.name().into()) }
}

/// Run the session until either side ends it.
pub(crate) async fn run(server: Arc<Server>, socket: WebSocket, session: Session) {
    let Session { account, mut role, workspace, project, mut expires_secs, room } = session;
    let conn = server.rooms.next_conn();
    let mut person = Person {
        id: account.id.clone(),
        name: account.name.clone(),
        role: role.name().into(),
        color: color_of(&account.id),
    };
    let (tx, mut rx) = mpsc::channel::<Outgoing>(PEER_QUEUE);
    room.join(conn, person.clone(), &account.id, role, tx);
    if let Ok(files) = server.workspaces.files(&workspace) {
        files.record(Event::ok(actor(&account, role), "project.join").target(project.clone()));
    }

    let (mut sink, mut stream) = socket.split();
    let (state_vector, presence) = room.welcome();
    let mut opening = vec![
        Frame::Control(Control::Welcome {
            you: person.clone(),
            project: project.clone(),
            read_only: role < Role::Operator,
            max_message_bytes: server.config.limits.max_message_bytes,
        }),
        Frame::SyncStep1(state_vector),
    ];
    if !presence.is_empty() {
        opening.push(Frame::Awareness(presence));
    }
    let mut open = true;
    for frame in opening {
        if sink.send(Message::Binary(frame.encode().into())).await.is_err() {
            open = false;
            break;
        }
    }

    let rule = Rule::per_second(server.config.limits.messages_per_second);
    let mut bucket = Bucket::full(rule);
    let mut strikes = 0u32;
    let mut close_reason: Option<String> = None;
    while open {
        let until_expiry = Duration::from_secs(expires_secs.saturating_sub(now_ms() / 1000));
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(Message::Binary(bytes))) => bytes,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Text(_))) => {
                        close_reason = Some("this protocol sends binary messages".into());
                        break;
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                };
                if !bucket.take(rule) {
                    strikes += 1;
                    if strikes > MAX_STRIKES {
                        close_reason = Some("too many messages".into());
                        break;
                    }
                    let refused = Frame::Control(Control::Refused { reason: "slow down: too many messages".into() });
                    if sink.send(Message::Binary(refused.encode().into())).await.is_err() { break }
                    continue;
                }
                let replies = handle(&server, &room, conn, &account, role, &person, &workspace, &project, &bytes, &mut expires_secs);
                let replies = match replies {
                    Ok(replies) => replies,
                    Err(Stop(reason)) => {
                        close_reason = Some(reason);
                        break;
                    }
                };
                for reply in replies {
                    if matches!(reply, Frame::Control(Control::Refused { .. })) {
                        strikes += 1;
                    }
                    if sink.send(Message::Binary(reply.encode().into())).await.is_err() {
                        open = false;
                        break;
                    }
                }
                if strikes > MAX_STRIKES {
                    close_reason = Some("too many refused changes".into());
                    break;
                }
            }
            outgoing = rx.recv() => match outgoing {
                Some(Outgoing::Frame(frame)) => {
                    if sink.send(Message::Binary(frame.encode().into())).await.is_err() { break }
                }
                Some(Outgoing::Role(new_role)) => {
                    role = new_role;
                    person.role = new_role.name().into();
                    if sink.send(Message::Binary(role_frame(new_role).encode().into())).await.is_err() { break }
                }
                Some(Outgoing::Close(reason)) => {
                    close_reason = Some(reason);
                    break;
                }
                None => break,
            },
            () = tokio::time::sleep(until_expiry) => {
                close_reason = Some("the access token expired".into());
                break;
            }
        }
    }
    if let Some(reason) = close_reason {
        let closing = Frame::Control(Control::Closing { reason: reason.clone() });
        let _gone = sink.send(Message::Binary(closing.encode().into())).await;
        let frame = CloseFrame { code: 4000, reason: Utf8Bytes::from(reason.chars().take(100).collect::<String>()) };
        let _gone = sink.send(Message::Close(Some(frame))).await;
    }
    if room.leave(conn) {
        server.rooms.close_when_idle(&room);
    }
    server.end_session(&account.id);
}

/// Why a session stops.
struct Stop(String);

/// Answer one message.
#[allow(clippy::too_many_arguments)]
fn handle(
    server: &Arc<Server>,
    room: &Room,
    conn: u64,
    account: &Account,
    role: Role,
    person: &Person,
    workspace: &str,
    project: &str,
    bytes: &[u8],
    expires_secs: &mut u64,
) -> Result<Vec<Frame>, Stop> {
    let refused = |reason: &str| Ok(vec![Frame::Control(Control::Refused { reason: reason.to_owned() })]);
    let frame = match Frame::decode(bytes) {
        Ok(frame) => frame,
        Err(error) => return refused(&error.to_string()),
    };
    match frame {
        Frame::SyncStep1(state_vector) => match room.diff(&state_vector) {
            Ok(update) => Ok(vec![Frame::SyncStep2(update)]),
            Err(error) => refused(&error.to_string()),
        },
        Frame::SyncStep2(update) | Frame::Update(update) => {
            if role < Role::Operator {
                // A viewer's replica answers the handshake too: only refuse real edits.
                return if room.is_new(&update) {
                    refused("you can view this project, not edit it")
                } else {
                    Ok(vec![])
                };
            }
            match room.apply(Some(conn), &account.name, &update) {
                Ok(super::room::Applied::Yes) => {
                    server.workspaces.touch_project(workspace, project);
                    server.project_changed(workspace, project, &account.id);
                    Ok(vec![])
                }
                Ok(super::room::Applied::Nothing) => Ok(vec![]),
                Err(ServerError::Limit(why)) => refused(&why),
                Err(ServerError::Io(why)) => {
                    eprintln!("warning: collab: {why}");
                    refused("the change could not be stored, try again")
                }
                Err(error) => refused(&error.to_string()),
            }
        }
        Frame::Awareness(entries) => {
            let refused_entries = room.presence(conn, person, entries);
            if refused_entries > 0 {
                refused("presence refused (too large, not an object, or another replica's)")
            } else {
                Ok(vec![])
            }
        }
        Frame::AwarenessQuery => Ok(vec![Frame::Awareness(room.all_presence())]),
        Frame::Auth(_) => Ok(vec![]),
        Frame::Control(Control::Token { token }) => match server.signing.verify(&token, now_ms() / 1000) {
            Ok(claims) if claims.sub == account.id && claims.ws.as_deref() == Some(workspace) => {
                *expires_secs = claims.exp;
                Ok(vec![])
            }
            _ => Err(Stop("the new access token is not valid for this session".into())),
        },
        Frame::Control(Control::SaveVersion { label }) => {
            if role < Role::Operator {
                return refused("you can view this project, not save versions");
            }
            match room.save_version(&label, &account.name) {
                Ok(version) => {
                    if let Ok(files) = server.workspaces.files(workspace) {
                        files.record(
                            Event::ok(actor(account, role), "version.save")
                                .target(project.to_owned())
                                .details(serde_json::json!({ "version": version.version, "label": version.label })),
                        );
                    }
                    server.emit_event(
                        workspace,
                        "version.saved",
                        Some(project),
                        serde_json::json!({ "version": version.version, "label": version.label, "by": account.name }),
                    );
                    room.broadcast_all(Frame::Control(Control::VersionSaved {
                        version: version.version,
                        label: version.label,
                        by: account.name.clone(),
                    }));
                    Ok(vec![])
                }
                Err(error) => refused(&error.to_string()),
            }
        }
        // Controls only the server sends.
        Frame::Control(_) => refused("this message is sent by the server only"),
    }
}

impl Server {
    /// Count a new session of `account`.
    ///
    /// @throws ServerError::TooMany - Over `limits.sessions_per_account`.
    pub(crate) fn begin_session(&self, account: &str) -> Result<(), ServerError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = sessions.entry(account.to_owned()).or_insert(0);
        if *count >= self.config.limits.sessions_per_account {
            return Err(ServerError::TooMany(format!(
                "at most {} open projects per account",
                self.config.limits.sessions_per_account
            )));
        }
        *count += 1;
        Ok(())
    }

    pub(crate) fn end_session(&self, account: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = sessions.get_mut(account) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sessions.remove(account);
            }
        }
    }

    /// Tell open sessions of `account` in `workspace` that its role changed, or (`None`) that it left.
    pub(crate) fn membership_changed(&self, workspace: &str, account: &str, role: Option<Role>) {
        for room in self.rooms.of_workspace(workspace) {
            let message = match role {
                Some(role) => Outgoing::Role(role),
                None => Outgoing::Close("you are no longer a member of this workspace".into()),
            };
            room.notify(Some(account), &message);
        }
    }

    /// Deliver a workspace event (`version.saved`, `member.joined`) to plugin services.
    pub(crate) fn emit_event(
        self: &Arc<Self>,
        workspace: &str,
        event: &'static str,
        project: Option<&str>,
        details: serde_json::Value,
    ) {
        #[cfg(feature = "services")]
        self.service_event(workspace, event, project, details);
        #[cfg(not(feature = "services"))]
        let _ = (workspace, event, project, details);
    }

    /// A project changed: plugin services that follow changes hear of it.
    pub(crate) fn project_changed(&self, workspace: &str, project: &str, by: &str) {
        #[cfg(feature = "services")]
        self.services.changed(self, workspace, project, by);
        #[cfg(not(feature = "services"))]
        let _ = (workspace, project, by);
    }
}
