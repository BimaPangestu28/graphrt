//! The sync protocol: binary WebSocket messages.
//!
//! The messages are those of the Yjs sync protocol (`y-protocols`), so any Yjs
//! client can speak it: sync step 1 (a state vector), sync step 2 (the updates
//! the other side lacks), updates, awareness (presence) and awareness queries.
//! Graphrt adds one custom message (tag [`CONTROL_TAG`]) holding a JSON
//! [`Control`]: who joined as what, a fresh access token, why a change was refused.
//!
//! A session goes:
//!
//! 1. The client opens the WebSocket with a workspace scoped access token.
//! 2. The server sends [`Control::Welcome`], then sync step 1 with its state vector.
//! 3. The client answers with sync step 2 (its offline edits) and sends its own
//!    sync step 1, the server answers with sync step 2.
//! 4. From then on both sides send updates and awareness as they happen.
//!    Before its token expires the client sends [`Control::Token`] with a new one.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yrs::sync::awareness::{AwarenessUpdate, AwarenessUpdateEntry};
use yrs::sync::{Message, SyncMessage};
use yrs::updates::decoder::Decode as _;
use yrs::updates::encoder::Encode as _;
use yrs::{ClientID, StateVector};

use crate::error::{CollabError, Result};

/// The tag of Graphrt's control messages (a Yjs custom message).
pub const CONTROL_TAG: u8 = 100;
/// The WebSocket subprotocol of this protocol.
pub const SUBPROTOCOL: &str = "graphrt-collab.v1";
/// Prefix of the subprotocol that carries the access token for clients that
/// cannot set an `Authorization` header (browsers): `bearer.<token>`.
pub const BEARER_SUBPROTOCOL_PREFIX: &str = "bearer.";

/// One presence entry: a replica's client id, its clock and its JSON state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub client_id: u64,
    pub clock: u32,
    /// The JSON state, `null` when the replica left.
    pub state: String,
}

/// A message of the protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// Sync step 1: the sender's encoded state vector.
    SyncStep1(Vec<u8>),
    /// Sync step 2: what the receiver lacks, as one update.
    SyncStep2(Vec<u8>),
    /// An update made after the handshake.
    Update(Vec<u8>),
    /// Presence of one or more replicas.
    Awareness(Vec<Presence>),
    /// A request for everyone's presence.
    AwarenessQuery,
    /// The Yjs auth message: `Some(reason)` when permission was denied.
    Auth(Option<String>),
    /// A Graphrt control message.
    Control(Control),
}

/// Who someone is, as the server tells everyone (never as a client claims).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// The account id on the server.
    pub id: String,
    pub name: String,
    /// `owner`, `operator` (can edit) or `viewer`.
    pub role: String,
    /// A color for avatars and cursors, the same for one account everywhere.
    pub color: String,
}

/// Graphrt's control messages, JSON in a Yjs custom message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Server -> client, first message: who the client is and whether it may edit.
    Welcome {
        you: Person,
        project: String,
        read_only: bool,
        /// The server's limit on one message, in bytes.
        max_message_bytes: usize,
    },
    /// Client -> server: a new access token, before the current one expires.
    Token { token: String },
    /// Server -> client: a change of the client was not applied.
    Refused { reason: String },
    /// Server -> client: the role of the client changed (`read_only` follows it).
    Role { role: String, read_only: bool },
    /// Client -> server: keep the current state as a named version.
    SaveVersion { label: String },
    /// Server -> clients: a version was saved.
    VersionSaved { version: u64, label: String, by: String },
    /// Server -> client: the session ends (expired token, removed from the workspace, server shutting down).
    Closing { reason: String },
}

impl Frame {
    /// Encode as one WebSocket binary message.
    pub fn encode(&self) -> Vec<u8> {
        let message = match self {
            Frame::SyncStep1(sv) => match StateVector::decode_v1(sv) {
                Ok(sv) => Message::Sync(SyncMessage::SyncStep1(sv)),
                Err(_) => Message::Sync(SyncMessage::SyncStep1(StateVector::default())),
            },
            Frame::SyncStep2(update) => Message::Sync(SyncMessage::SyncStep2(update.clone())),
            Frame::Update(update) => Message::Sync(SyncMessage::Update(update.clone())),
            Frame::Awareness(entries) => Message::Awareness(AwarenessUpdate {
                clients: entries
                    .iter()
                    .map(|p| {
                        let entry = AwarenessUpdateEntry { clock: p.clock, json: Arc::from(p.state.as_str()) };
                        (ClientID::new(p.client_id), entry)
                    })
                    .collect::<HashMap<_, _>>(),
            }),
            Frame::AwarenessQuery => Message::AwarenessQuery,
            Frame::Auth(reason) => Message::Auth(reason.clone()),
            Frame::Control(control) => Message::Custom(CONTROL_TAG, serde_json::to_vec(control).unwrap_or_default()),
        };
        message.encode_v1()
    }

    /// Decode one WebSocket binary message.
    ///
    /// @throws CollabError::Protocol - When `bytes` is not a message of this protocol.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let message = Message::decode_v1(bytes).map_err(|error| CollabError::Protocol(error.to_string()))?;
        Ok(match message {
            Message::Sync(SyncMessage::SyncStep1(sv)) => Frame::SyncStep1(sv.encode_v1()),
            Message::Sync(SyncMessage::SyncStep2(update)) => Frame::SyncStep2(update),
            Message::Sync(SyncMessage::Update(update)) => Frame::Update(update),
            Message::Awareness(update) => Frame::Awareness(
                update
                    .clients
                    .into_iter()
                    .map(|(id, entry)| Presence {
                        client_id: id.get(),
                        clock: entry.clock,
                        state: entry.json.to_string(),
                    })
                    .collect(),
            ),
            Message::AwarenessQuery => Frame::AwarenessQuery,
            Message::Auth(reason) => Frame::Auth(reason),
            Message::Custom(CONTROL_TAG, body) => Frame::Control(
                serde_json::from_slice(&body).map_err(|error| CollabError::Protocol(format!("control: {error}")))?,
            ),
            Message::Custom(tag, _) => return Err(CollabError::Protocol(format!("unknown message tag {tag}"))),
        })
    }
}

/// The color of an account's avatar: one of a fixed palette, picked by a hash of its id.
pub fn color_of(account_id: &str) -> String {
    const PALETTE: [&str; 10] =
        ["#e5484d", "#f76b15", "#ffc53d", "#46a758", "#12a594", "#0090ff", "#3e63dd", "#8e4ec6", "#d6409f", "#978365"];
    // FNV-1a: stable across builds and platforms.
    let hash =
        account_id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    let index = usize::try_from(hash % PALETTE.len() as u64).unwrap_or(0);
    PALETTE.get(index).copied().unwrap_or("#0090ff").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectDoc;

    #[test]
    fn frames_round_trip() {
        let doc = ProjectDoc::new();
        let frames = vec![
            Frame::SyncStep1(doc.state_vector()),
            Frame::SyncStep2(doc.encode_state()),
            Frame::Update(vec![0, 0]),
            Frame::Awareness(vec![Presence { client_id: 7, clock: 3, state: r#"{"focus":"llm"}"#.into() }]),
            Frame::AwarenessQuery,
            Frame::Auth(Some("no".into())),
            Frame::Control(Control::Token { token: "t".into() }),
            Frame::Control(Control::Welcome {
                you: Person { id: "a".into(), name: "Ann".into(), role: "viewer".into(), color: color_of("a") },
                project: "p".into(),
                read_only: true,
                max_message_bytes: 1024,
            }),
        ];
        for frame in frames {
            assert_eq!(Frame::decode(&frame.encode()).unwrap(), frame);
        }
    }

    #[test]
    fn refuses_unknown_messages() {
        assert!(Frame::decode(&[]).is_err());
        assert!(Frame::decode(&[9, 9, 9]).is_err());
        let custom = Message::Custom(42, vec![1]).encode_v1();
        assert!(Frame::decode(&custom).is_err());
        let bad_json = Message::Custom(CONTROL_TAG, b"{".to_vec()).encode_v1();
        assert!(Frame::decode(&bad_json).is_err());
    }

    #[test]
    fn colors_are_stable() {
        assert_eq!(color_of("acct_1"), color_of("acct_1"));
        assert!(color_of("x").starts_with('#'));
    }
}
