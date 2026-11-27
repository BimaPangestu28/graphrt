//! Slack, through the [Events API](https://api.slack.com/apis/events-api) or
//! [Socket Mode](https://api.slack.com/apis/socket-mode), and the Web API.
//!
//! An app receives in one of two ways:
//!
//! - **Socket Mode** (feature `socket-mode`, [`SocketMode`]): the app opens a
//!   WebSocket to Slack with its app level token (`xapp-...`). No public address
//!   is needed, so it works on a laptop or behind a company firewall.
//! - **Events API**: Slack posts each event to an HTTPS address. Every delivery
//!   is signed with the app's signing secret: check it with [`verify_signature`]
//!   on the raw body, then read it with [`parse_delivery`]. Button taps
//!   (interactivity) arrive at the same kind of address, form encoded.
//!
//! What a host sees:
//!
//! | In Slack | [`InboundMessage`] |
//! | --- | --- |
//! | a direct message to the app | `chat_id` is the DM channel, [`ChatKind::Private`] |
//! | a message in a thread of a DM | `chat_id` is `channel:thread_ts` |
//! | a mention of the app in a channel | `chat_id` is `channel:ts`: the answer starts a thread, and the thread is the conversation |
//! | a mention inside a thread | `chat_id` is `channel:thread_ts`, the same conversation again |
//! | a tap on a button the app sent | `choice` is the button's id |
//!
//! Plain messages in a channel that do not mention the app are not for it and
//! are [`InboundEvent::Ignored`]. So are messages of bots, edits and deletions.

use serde_json::Value as Json;

use crate::model::{
    Attachment, AttachmentKind, Button, ChannelError, ChatKind, Customer, InboundEvent, InboundMessage, Secret,
};
use crate::sign::{constant_time_eq, from_hex, hmac_sha256, to_hex};
use crate::text;

/// The header with the signature of a delivery (`v0=<hex>`).
pub const SIGNATURE_HEADER: &str = "x-slack-signature";
/// The header with the time the delivery was signed, in Unix seconds.
pub const TIMESTAMP_HEADER: &str = "x-slack-request-timestamp";
/// How far a delivery's timestamp may be from now before it counts as a replay.
pub const MAX_SKEW_SECS: u64 = 5 * 60;
/// The prefix of the `action_id` of the buttons this crate sends.
pub const ACTION_PREFIX: &str = "graphrt_choice";
/// Most blocks in one message.
pub const MAX_BLOCKS: usize = 50;
/// Most buttons in one actions block.
const MAX_BUTTONS: usize = 25;
/// Longest text of a section block.
const SECTION_CHARS: usize = 3000;
/// Longest text of a button.
const BUTTON_CHARS: usize = 75;
/// Longest text of a header block.
const HEADER_CHARS: usize = 150;
/// Most fields of one section block.
const MAX_FIELDS: usize = 10;

/// The signature Slack sends with `body` signed at `timestamp`: `v0=<hex>`.
/// For tests and for a host that forwards deliveries.
pub fn sign(body: &[u8], timestamp: &str, signing_secret: &Secret) -> String {
    let mut base = format!("v0:{timestamp}:").into_bytes();
    base.extend_from_slice(body);
    format!("v0={}", to_hex(&hmac_sha256(signing_secret.expose().as_bytes(), &base)))
}

/// Check the signature of a delivery: `signature` and `timestamp` are the
/// values of [`SIGNATURE_HEADER`] and [`TIMESTAMP_HEADER`], `now_secs` the
/// current Unix time.
///
/// @throws ChannelError - `MissingSignature` without the headers, `Stale` when
///   the timestamp is more than [`MAX_SKEW_SECS`] away, `BadSignature` otherwise.
pub fn verify_signature(
    body: &[u8],
    timestamp: Option<&str>,
    signature: Option<&str>,
    signing_secret: &Secret,
    now_secs: u64,
) -> Result<(), ChannelError> {
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return Err(ChannelError::MissingSignature("X-Slack-Signature"));
    };
    if signing_secret.is_empty() {
        return Err(ChannelError::BadSignature("X-Slack-Signature"));
    }
    let Ok(signed_at) = timestamp.trim().parse::<u64>() else {
        return Err(ChannelError::BadSignature("X-Slack-Request-Timestamp"));
    };
    if signed_at.abs_diff(now_secs) > MAX_SKEW_SECS {
        return Err(ChannelError::Stale);
    }
    let given = signature.trim().strip_prefix("v0=").and_then(from_hex);
    let expected = sign(body, timestamp.trim(), signing_secret);
    let expected = expected.strip_prefix("v0=").and_then(from_hex).unwrap_or_default();
    match given {
        Some(given) if constant_time_eq(&given, &expected) => Ok(()),
        _ => Err(ChannelError::BadSignature("X-Slack-Signature")),
    }
}

/// One delivery of the Events API, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Slack checks the address (`url_verification`): answer with this text.
    Challenge(String),
    /// An event or a button tap.
    Event(InboundEvent),
}

/// Read a verified delivery: the JSON of an event, or the form encoded
/// `payload=` of a button tap.
///
/// @throws ChannelError - `Invalid` when the body is neither.
pub fn parse_delivery(body: &[u8]) -> Result<Delivery, ChannelError> {
    let invalid = || ChannelError::Invalid("the delivery is not a Slack event".to_owned());
    if let Some(form) = body.strip_prefix(b"payload=") {
        let decoded = form_value(form).ok_or_else(invalid)?;
        let payload: Json = serde_json::from_str(&decoded).map_err(|_| invalid())?;
        return Ok(Delivery::Event(parse_interaction(&payload)));
    }
    let payload: Json = serde_json::from_slice(body).map_err(|_| invalid())?;
    match payload.get("type").and_then(Json::as_str) {
        Some("url_verification") => {
            let challenge = payload.get("challenge").and_then(Json::as_str).ok_or_else(invalid)?;
            Ok(Delivery::Challenge(challenge.to_owned()))
        }
        Some("event_callback") => Ok(Delivery::Event(parse_event_callback(&payload))),
        _ => Ok(Delivery::Event(InboundEvent::Ignored("not an event"))),
    }
}

/// A form value: `+` is a space, `%XX` a byte. `None` when it is not UTF-8.
fn form_value(raw: &[u8]) -> Option<String> {
    let raw = raw.split(|byte| *byte == b'&').next().unwrap_or_default();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while let Some(byte) = raw.get(index).copied() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(raw.get(index + 1..index + 3)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                index += 2;
            }
            other => bytes.push(other),
        }
        index += 1;
    }
    String::from_utf8(bytes).ok()
}

fn text_of<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).filter(|text| !text.is_empty())
}

/// Whether `channel` is a direct message channel.
fn is_direct(channel: &str, channel_type: Option<&str>) -> bool {
    channel_type == Some("im") || channel.starts_with('D')
}

/// The chat of a message: the DM itself, or `channel:thread`.
fn chat_of(channel: &str, direct: bool, thread_ts: Option<&str>, ts: &str) -> String {
    match (direct, thread_ts) {
        (true, None) => channel.to_owned(),
        (true, Some(thread)) => format!("{channel}:{thread}"),
        (false, thread) => format!("{channel}:{}", thread.unwrap_or(ts)),
    }
}

/// The channel and the thread of a `chat_id`.
pub fn split_chat(chat_id: &str) -> (&str, Option<&str>) {
    match chat_id.split_once(':') {
        Some((channel, thread)) if !thread.is_empty() => (channel, Some(thread)),
        _ => (chat_id, None),
    }
}

/// Slack's `ts` (`1700000000.000100`) in milliseconds.
fn ts_ms(ts: &str) -> Option<u64> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let seconds: u64 = seconds.parse().ok()?;
    let millis: u64 = fraction.get(..3).and_then(|digits| digits.parse().ok()).unwrap_or(0);
    Some(seconds.saturating_mul(1000).saturating_add(millis))
}

/// Slack's message text as the customer wrote it: mentions of the app at the
/// start removed, links and mentions written out, entities decoded.
pub fn plain_text(slack: &str) -> String {
    let mut out = String::with_capacity(slack.len());
    let mut rest = slack.trim_start();
    // `<@U123> hello`: the mention that addressed the app is not part of the question.
    while let Some(after) = rest.strip_prefix("<@") {
        match after.split_once('>') {
            Some((_, tail)) => rest = tail.trim_start(),
            None => break,
        }
    }
    while let Some(open) = rest.find('<') {
        out.push_str(rest.get(..open).unwrap_or_default());
        let after = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = after.find('>') else {
            out.push_str(rest.get(open..).unwrap_or_default());
            rest = "";
            break;
        };
        let inner = after.get(..close).unwrap_or_default();
        let (target, label) = inner.split_once('|').unwrap_or((inner, ""));
        let written = match (target.chars().next(), label.is_empty()) {
            (Some('@' | '#'), false) => format!("{}{label}", &target[..1]),
            (Some('@' | '#'), true) => target.to_owned(),
            (Some('!'), _) => format!("@{}", target.trim_start_matches('!')),
            (_, true) => target.to_owned(),
            (_, false) if label == target => target.to_owned(),
            (_, false) => format!("{label} ({target})"),
        };
        out.push_str(&written);
        rest = after.get(close + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&").trim().to_owned()
}

fn attachment_kind(mime_type: Option<&str>) -> AttachmentKind {
    match mime_type.unwrap_or_default().split('/').next() {
        Some("image") => AttachmentKind::Image,
        Some("audio") => AttachmentKind::Audio,
        Some("video") => AttachmentKind::Video,
        _ => AttachmentKind::Document,
    }
}

/// The files of a message. The `file_id` is the file's private download URL,
/// which [`SlackBot`] fetches with the bot token (only from Slack's file host).
fn files_of(event: &Json) -> Vec<Attachment> {
    let Some(files) = event.get("files").and_then(Json::as_array) else { return Vec::new() };
    files
        .iter()
        .filter_map(|file| {
            let url = text_of(file, "url_private_download").or_else(|| text_of(file, "url_private"))?;
            let mime_type = text_of(file, "mimetype").map(str::to_owned);
            Some(Attachment {
                file_id: url.to_owned(),
                kind: attachment_kind(mime_type.as_deref()),
                name: text_of(file, "name").map(str::to_owned),
                mime_type,
                size: file.get("size").and_then(Json::as_u64),
            })
        })
        .collect()
}

/// Read an `event_callback` (the body of an Events API delivery, or the
/// payload of a Socket Mode `events_api` envelope).
pub fn parse_event_callback(payload: &Json) -> InboundEvent {
    let Some(event) = payload.get("event") else { return InboundEvent::Ignored("no event") };
    let kind = text_of(event, "type");
    if !matches!(kind, Some("message" | "app_mention")) {
        return InboundEvent::Ignored("not a message");
    }
    if event.get("bot_id").is_some() || event.get("bot_profile").is_some() {
        return InboundEvent::Ignored("written by a bot");
    }
    // Edits, deletions, joins: everything with a subtype but a shared file.
    if text_of(event, "subtype").is_some_and(|subtype| subtype != "file_share") {
        return InboundEvent::Ignored("not a new message");
    }
    let (Some(channel), Some(user), Some(ts)) =
        (text_of(event, "channel"), text_of(event, "user"), text_of(event, "ts"))
    else {
        return InboundEvent::Ignored("a message without ids");
    };
    let direct = is_direct(channel, text_of(event, "channel_type"));
    if kind == Some("message") && !direct {
        // In a channel the app answers when it is mentioned: that arrives as `app_mention`.
        return InboundEvent::Ignored("a channel message that does not mention the app");
    }
    let thread_ts = text_of(event, "thread_ts");
    let name = event
        .get("user_profile")
        .and_then(|profile| text_of(profile, "real_name").or_else(|| text_of(profile, "display_name")))
        .map(str::to_owned);
    let text = plain_text(text_of(event, "text").unwrap_or_default());
    let attachments = files_of(event);
    if text.is_empty() && attachments.is_empty() {
        return InboundEvent::Ignored("an empty message");
    }
    InboundEvent::Message(Box::new(InboundMessage {
        // One id for the `message` and the `app_mention` of the same message.
        id: format!("{channel}:{ts}"),
        chat_id: chat_of(channel, direct, thread_ts, ts),
        chat_kind: if direct { ChatKind::Private } else { ChatKind::Group },
        customer: Customer { id: user.to_owned(), name },
        text,
        attachments,
        choice: None,
        reference: None,
        timestamp_ms: ts_ms(ts),
        unsupported: None,
    }))
}

/// Read an interaction payload: a tap on a button of this crate (`block_actions`).
pub fn parse_interaction(payload: &Json) -> InboundEvent {
    if text_of(payload, "type") != Some("block_actions") {
        return InboundEvent::Ignored("not a button tap");
    }
    let Some(action) = payload
        .get("actions")
        .and_then(Json::as_array)
        .and_then(|actions| actions.first())
        .filter(|action| text_of(action, "action_id").is_some_and(|id| id.starts_with(ACTION_PREFIX)))
    else {
        return InboundEvent::Ignored("a tap on a button of another app");
    };
    let user = payload.get("user").and_then(|user| text_of(user, "id"));
    let channel = payload
        .get("channel")
        .and_then(|channel| text_of(channel, "id"))
        .or_else(|| payload.get("container").and_then(|container| text_of(container, "channel_id")));
    let message = payload.get("message").unwrap_or(&Json::Null);
    let (Some(user), Some(channel), Some(value), Some(message_ts)) =
        (user, channel, text_of(action, "value"), text_of(message, "ts"))
    else {
        return InboundEvent::Ignored("a tap without ids");
    };
    let thread_ts = text_of(message, "thread_ts")
        .or_else(|| payload.get("container").and_then(|container| text_of(container, "thread_ts")));
    let direct = is_direct(channel, None);
    let title = action.get("text").and_then(|text| text_of(text, "text")).unwrap_or(value);
    let action_ts = text_of(action, "action_ts").unwrap_or(message_ts);
    // Where to say the tap was taken: Slack's `response_url`, only ever on its own host.
    let reference = text_of(payload, "response_url")
        .filter(|url| url.starts_with("https://hooks.slack.com/"))
        .map(|url| format!("{url}\n{}", text_of(message, "text").unwrap_or_default()));
    InboundEvent::Message(Box::new(InboundMessage {
        id: format!("tap:{channel}:{action_ts}:{user}"),
        chat_id: chat_of(channel, direct, thread_ts, message_ts),
        chat_kind: if direct { ChatKind::Private } else { ChatKind::Group },
        customer: Customer {
            id: user.to_owned(),
            name: payload.get("user").and_then(|u| text_of(u, "name")).map(str::to_owned),
        },
        text: title.to_owned(),
        attachments: Vec::new(),
        choice: Some(value.to_owned()),
        reference,
        timestamp_ms: ts_ms(action_ts),
        unsupported: None,
    }))
}

// ---------------------------------------------------------------- Block Kit

fn section(mrkdwn: &str) -> Json {
    serde_json::json!({ "type": "section", "text": { "type": "mrkdwn", "text": mrkdwn } })
}

/// Markdown as section blocks of `mrkdwn`, each within Slack's limit.
pub fn markdown_blocks(markdown: &str) -> Vec<Json> {
    text::split(markdown, SECTION_CHARS - 200)
        .iter()
        .map(|part| section(&text::truncate(&text::to_slack_mrkdwn(part), SECTION_CHARS)))
        .collect()
}

/// An actions block with one button per entry, or `None` without buttons or
/// with more than Slack shows in one block (the choices are then in the text).
pub fn buttons_block(buttons: &[Button], links: &[(String, String)]) -> Option<Json> {
    if (buttons.is_empty() && links.is_empty()) || buttons.len() + links.len() > MAX_BUTTONS {
        return None;
    }
    let mut elements: Vec<Json> = buttons
        .iter()
        .enumerate()
        .map(|(index, button)| {
            serde_json::json!({
                "type": "button",
                "action_id": format!("{ACTION_PREFIX}_{index}"),
                "text": { "type": "plain_text", "text": text::truncate(&button.title, BUTTON_CHARS), "emoji": true },
                "value": text::truncate(&button.id, 2000),
            })
        })
        .collect();
    elements.extend(links.iter().enumerate().map(|(index, (title, url))| {
        serde_json::json!({
            "type": "button",
            "action_id": format!("graphrt_link_{index}"),
            "text": { "type": "plain_text", "text": text::truncate(title, BUTTON_CHARS), "emoji": true },
            "url": url,
        })
    }));
    Some(serde_json::json!({ "type": "actions", "elements": elements }))
}

/// What a card's body turns into.
#[derive(Default)]
struct CardBlocks {
    blocks: Vec<Json>,
    links: Vec<(String, String)>,
}

impl CardBlocks {
    /// Read one element. `false` when it is one Block Kit cannot show.
    fn element(&mut self, element: &Json, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        match element.get("type").and_then(Json::as_str) {
            Some("TextBlock") => {
                let Some(text) = text_of(element, "text") else { return true };
                let large = matches!(element.get("size").and_then(Json::as_str), Some("large" | "extraLarge"));
                if large && !text.contains('\n') {
                    self.blocks.push(serde_json::json!({
                        "type": "header",
                        "text": { "type": "plain_text", "text": text::truncate(text, HEADER_CHARS), "emoji": true },
                    }));
                } else if element.get("weight").and_then(Json::as_str) == Some("bolder") {
                    self.blocks.push(section(&format!("*{}*", text::slack_escape(text))));
                } else {
                    self.blocks.extend(markdown_blocks(text));
                }
                true
            }
            Some("RichTextBlock") => {
                let inlines = element.get("inlines").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                let line: String = inlines
                    .iter()
                    .map(|inline| inline.as_str().or_else(|| text_of(inline, "text")).unwrap_or_default())
                    .collect();
                if !line.trim().is_empty() {
                    self.blocks.push(section(&text::slack_escape(&line)));
                }
                true
            }
            Some("FactSet") => {
                let facts = element.get("facts").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                let fields: Vec<Json> = facts
                    .iter()
                    .map(|fact| {
                        let title = text::slack_escape(text_of(fact, "title").unwrap_or_default());
                        let value = text::slack_escape(text_of(fact, "value").unwrap_or_default());
                        serde_json::json!({ "type": "mrkdwn", "text": text::truncate(&format!("*{title}*\n{value}"), 2000) })
                    })
                    .collect();
                for chunk in fields.chunks(MAX_FIELDS) {
                    self.blocks.push(serde_json::json!({ "type": "section", "fields": chunk }));
                }
                true
            }
            Some("Image") => {
                // Slack shows images from public `https` addresses only: a `data:` image is left out.
                if let Some(url) = text_of(element, "url").filter(|url| url.starts_with("https://")) {
                    let alt = text_of(element, "altText").unwrap_or("image");
                    self.blocks.push(
                        serde_json::json!({ "type": "image", "image_url": url, "alt_text": text::truncate(alt, 2000) }),
                    );
                }
                true
            }
            Some("ImageSet") => {
                let images = element.get("images").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                images.iter().all(|image| self.element(image, depth + 1))
            }
            Some("Container" | "Column") => {
                let items = element.get("items").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                items.iter().all(|item| self.element(item, depth + 1))
            }
            Some("ColumnSet") => {
                let columns = element.get("columns").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                columns.iter().all(|column| self.element(column, depth + 1))
            }
            Some("ActionSet") => {
                let actions = element.get("actions").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                self.actions(actions)
            }
            // The choices become buttons: the input itself is not drawn.
            Some("Input.ChoiceSet") => true,
            _ => false,
        }
    }

    fn actions(&mut self, actions: &[Json]) -> bool {
        actions.iter().all(|action| match action.get("type").and_then(Json::as_str) {
            Some("Action.OpenUrl") => {
                if let Some(url) = text_of(action, "url").filter(|url| url.starts_with("https://")) {
                    let title = text_of(action, "title").unwrap_or(url);
                    self.links.push((title.to_owned(), url.to_owned()));
                }
                true
            }
            Some("Action.Submit" | "Action.Execute") => true,
            _ => false,
        })
    }
}

/// An Adaptive Card as Block Kit: text blocks as sections (a large one as a
/// header), facts as fields, `https` images, containers and columns read in
/// order, links and `choices` as buttons. `None` when the card has something
/// Block Kit cannot show (a text field, a toggle, a card that shows another
/// card): send its fallback text then.
pub fn card_blocks(card: &Json, choices: &[Button]) -> Option<Vec<Json>> {
    if !crate::card::buttons_can_answer(card, choices.len()) {
        return None;
    }
    let mut built = CardBlocks::default();
    let body = card.get("body").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
    if !body.iter().all(|element| built.element(element, 0)) {
        return None;
    }
    let actions = card.get("actions").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
    if !built.actions(actions) {
        return None;
    }
    let has_buttons = !choices.is_empty() || !built.links.is_empty();
    match buttons_block(choices, &built.links) {
        Some(block) => built.blocks.push(block),
        None if has_buttons => return None,
        None => {}
    }
    (!built.blocks.is_empty() && built.blocks.len() <= MAX_BLOCKS).then_some(built.blocks)
}

#[cfg(feature = "client")]
pub use client::{SlackBot, SlackIdentity};
#[cfg(feature = "socket-mode")]
pub use socket::{SocketConnection, SocketEvent, SocketMode};

#[cfg(feature = "client")]
mod client {
    use std::time::Duration;

    use serde_json::{Value as Json, json};

    use super::{MAX_BLOCKS, buttons_block, card_blocks, markdown_blocks, split_chat, text_of};
    use crate::channel::{BoxFuture, Channel, Download, OutboundFile, RichMessage, RichPart};
    use crate::http::{self, Answer, CALL_TIMEOUT, MAX_ANSWER_BYTES, TRANSFER_TIMEOUT, form, safe_file_name};
    use crate::model::{Attachment, Button, ChannelError, ChannelKind, InboundMessage, Secret};
    use crate::text;

    /// Where the Web API lives.
    pub const API_BASE: &str = "https://slack.com/api";
    /// Where Slack keeps the files customers share.
    const FILE_HOST: &str = "https://files.slack.com/";
    /// Largest file the app uploads.
    const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
    /// Longest wait Slack may ask for that a call still honours by itself.
    const MAX_RETRY_AFTER_SECS: u64 = 30;
    const RETRIES: usize = 2;
    const KIND: ChannelKind = ChannelKind::Slack;

    /// Who the app's bot user is (`auth.test`).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SlackIdentity {
        pub user_id: String,
        pub team: Option<String>,
    }

    /// One Slack app, through its bot token (`xoxb-...`).
    #[derive(Clone, Debug)]
    pub struct SlackBot {
        http: reqwest::Client,
        base: String,
        token: Secret,
    }

    impl SlackBot {
        /// The app of the bot token `token`, calling through `http`.
        ///
        /// @throws ChannelError - `Invalid` when `token` is not a bot token.
        pub fn new(http: reqwest::Client, token: Secret) -> Result<Self, ChannelError> {
            let shaped = token.expose().starts_with("xoxb-")
                && token.expose().len() >= 16
                && token.expose().bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !shaped {
                return Err(ChannelError::Invalid(
                    "the Slack bot token is not one (expected xoxb-..., from OAuth & Permissions of the app)"
                        .to_owned(),
                ));
            }
            Ok(Self { http, base: API_BASE.to_owned(), token })
        }

        /// Call another API server (a fake in tests).
        pub fn with_base_url(mut self, base: &str) -> Self {
            self.base = base.trim_end_matches('/').to_owned();
            self
        }

        /// Whether `url` is one the bot token may be sent to: Slack's file host,
        /// or the API server this bot calls.
        fn may_carry_token(&self, url: &str) -> bool {
            let origin = self.base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
            url.starts_with(FILE_HOST) || (!origin.is_empty() && url.starts_with(&format!("{origin}/")))
        }

        fn refusal(method: &str, answer: &Answer, body: &Json) -> ChannelError {
            let error = text_of(body, "error").unwrap_or("no description");
            ChannelError::Api {
                channel: KIND,
                method: method.to_owned(),
                status: answer.status,
                code: None,
                description: text::truncate(error, 300),
                retry_after: answer.retry_after,
            }
        }

        fn bad_answer(method: &str, reason: &str) -> ChannelError {
            ChannelError::BadAnswer { channel: KIND, method: method.to_owned(), reason: reason.to_owned() }
        }

        /// Call `method` with `body` (JSON, or a form when `form_body` is set).
        /// A `429` with a short `Retry-After` is waited out.
        async fn call_with(&self, method: &str, body: Vec<u8>, content_type: &str) -> Result<Json, ChannelError> {
            let mut attempt = 0;
            loop {
                let request = self
                    .http
                    .post(format!("{}/{method}", self.base))
                    .bearer_auth(self.token.expose())
                    .header(reqwest::header::CONTENT_TYPE, content_type)
                    .body(body.clone());
                let answer = http::send(KIND, request, CALL_TIMEOUT, MAX_ANSWER_BYTES).await?;
                let parsed = answer.json();
                if answer.status == 200 && parsed.get("ok").and_then(Json::as_bool) == Some(true) {
                    return Ok(parsed);
                }
                let refused = Self::refusal(method, &answer, &parsed);
                match answer.retry_after {
                    Some(wait) if answer.status == 429 && attempt < RETRIES && wait <= MAX_RETRY_AFTER_SECS => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
                    }
                    _ => return Err(refused),
                }
            }
        }

        async fn call(&self, method: &str, params: &Json) -> Result<Json, ChannelError> {
            let body = serde_json::to_vec(params).map_err(|e| ChannelError::Invalid(e.to_string()))?;
            self.call_with(method, body, "application/json; charset=utf-8").await
        }

        /// Who the token belongs to: a check that it works.
        pub async fn identity(&self) -> Result<SlackIdentity, ChannelError> {
            let answer = self.call("auth.test", &json!({})).await?;
            let user_id = text_of(&answer, "user_id").ok_or_else(|| Self::bad_answer("auth.test", "no user id"))?;
            Ok(SlackIdentity { user_id: user_id.to_owned(), team: text_of(&answer, "team").map(str::to_owned) })
        }

        /// Post one message of `blocks` to `chat_id` (a channel, a user, or
        /// `channel:thread_ts`). `text` is what notifications show.
        ///
        /// @returns The message's `ts`.
        pub async fn post_message(&self, chat_id: &str, text: &str, blocks: &[Json]) -> Result<String, ChannelError> {
            let (channel, thread) = split_chat(chat_id);
            let mut params = json!({
                "channel": channel,
                "text": text::truncate(text, 3000),
                "unfurl_links": false,
                "unfurl_media": false,
            });
            if let Some(object) = params.as_object_mut() {
                if !blocks.is_empty() {
                    object.insert("blocks".to_owned(), Json::Array(blocks.to_vec()));
                }
                if let Some(thread) = thread {
                    object.insert("thread_ts".to_owned(), json!(thread));
                }
            }
            let sent = self.call("chat.postMessage", &params).await?;
            text_of(&sent, "ts").map(str::to_owned).ok_or_else(|| Self::bad_answer("chat.postMessage", "no ts"))
        }

        /// Post `blocks` in as many messages as Slack's block limit needs.
        async fn post_blocks(&self, chat_id: &str, text: &str, blocks: Vec<Json>) -> Result<Vec<String>, ChannelError> {
            let mut sent = Vec::new();
            for chunk in blocks.chunks(MAX_BLOCKS) {
                sent.push(self.post_message(chat_id, text, chunk).await?);
            }
            Ok(sent)
        }

        async fn send_parts(
            &self,
            chat_id: &str,
            markdown: &str,
            buttons: &[Button],
        ) -> Result<Vec<String>, ChannelError> {
            let mut blocks = markdown_blocks(markdown);
            match buttons_block(buttons, &[]) {
                Some(block) => blocks.push(block),
                // More choices than one block shows: they are numbered in the text already.
                None if blocks.is_empty() => return Ok(Vec::new()),
                None => {}
            }
            self.post_blocks(chat_id, &text::to_slack_mrkdwn(markdown), blocks).await
        }

        async fn rich(&self, chat_id: &str, message: &RichMessage) -> Result<Vec<String>, ChannelError> {
            let mut blocks = Vec::new();
            for part in &message.parts {
                match part {
                    RichPart::Markdown(markdown) => blocks.extend(markdown_blocks(markdown)),
                    RichPart::Card { card, fallback, choices } => match card_blocks(card, choices) {
                        Some(drawn) => blocks.extend(drawn),
                        None => {
                            blocks.extend(markdown_blocks(fallback));
                            blocks.extend(buttons_block(choices, &[]));
                        }
                    },
                }
            }
            blocks.extend(buttons_block(&message.buttons, &[]));
            if blocks.is_empty() {
                return Ok(Vec::new());
            }
            self.post_blocks(chat_id, &text::to_slack_mrkdwn(&message.markdown()), blocks).await
        }

        async fn upload(&self, chat_id: &str, file: &OutboundFile) -> Result<String, ChannelError> {
            if file.bytes.len() > MAX_UPLOAD_BYTES {
                return Err(ChannelError::TooLarge { limit: u64::try_from(MAX_UPLOAD_BYTES).unwrap_or(u64::MAX) });
            }
            let name = safe_file_name(&file.name);
            let length = file.bytes.len().to_string();
            let body = form(&[("filename", &name), ("length", &length)]).into_bytes();
            let method = "files.getUploadURLExternal";
            let ticket = self.call_with(method, body, "application/x-www-form-urlencoded").await?;
            let (Some(url), Some(file_id)) = (text_of(&ticket, "upload_url"), text_of(&ticket, "file_id")) else {
                return Err(Self::bad_answer(method, "no upload address"));
            };
            if !url.starts_with(FILE_HOST) && !self.may_carry_token(url) {
                return Err(Self::bad_answer(method, "an upload address that is not Slack's"));
            }
            let request = self
                .http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, file.mime_type.as_str())
                .body(file.bytes.clone());
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, MAX_ANSWER_BYTES).await?;
            if answer.status != 200 {
                return Err(ChannelError::Api {
                    channel: KIND,
                    method: "file upload".to_owned(),
                    status: answer.status,
                    code: None,
                    description: "the file was not taken".to_owned(),
                    retry_after: answer.retry_after,
                });
            }
            let (channel, thread) = split_chat(chat_id);
            let mut params = json!({ "files": [{ "id": file_id, "title": name }], "channel_id": channel });
            if let Some(object) = params.as_object_mut() {
                if let Some(thread) = thread {
                    object.insert("thread_ts".to_owned(), json!(thread));
                }
                if let Some(caption) = file.caption.as_deref().filter(|caption| !caption.trim().is_empty()) {
                    object.insert("initial_comment".to_owned(), json!(text::truncate(caption.trim(), 3000)));
                }
            }
            self.call("files.completeUploadExternal", &params).await?;
            Ok(file_id.to_owned())
        }

        async fn fetch(&self, attachment: &Attachment, max_bytes: u64) -> Result<Download, ChannelError> {
            if attachment.size.is_some_and(|size| size > max_bytes) {
                return Err(ChannelError::TooLarge { limit: max_bytes });
            }
            // The token goes only to Slack's own file host.
            if !self.may_carry_token(&attachment.file_id) {
                return Err(Self::bad_answer("file download", "a file address that is not Slack's"));
            }
            let request = self.http.get(&attachment.file_id).bearer_auth(self.token.expose());
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, max_bytes).await?;
            // Without the `files:read` scope Slack answers with its sign-in page.
            let is_page = answer.content_type.as_deref().is_some_and(|kind| kind.starts_with("text/html"));
            if answer.status != 200 || is_page {
                return Err(ChannelError::Api {
                    channel: KIND,
                    method: "file download".to_owned(),
                    status: answer.status,
                    code: None,
                    description: "the file could not be downloaded (does the app have the files:read scope?)"
                        .to_owned(),
                    retry_after: None,
                });
            }
            Ok(Download { bytes: answer.body, mime_type: attachment.mime_type.clone(), name: attachment.name.clone() })
        }

        /// Replace the message whose button was tapped with its text and the
        /// choice, so its buttons cannot be tapped twice.
        async fn mark_tapped(&self, message: &InboundMessage) -> Result<(), ChannelError> {
            let Some((url, original)) = message.reference.as_deref().and_then(|r| r.split_once('\n')) else {
                return Ok(());
            };
            if !url.starts_with("https://hooks.slack.com/") && !self.may_carry_token(url) {
                return Ok(());
            }
            let chosen = format!("{original}\n\n> {}", text::slack_escape(&message.text));
            let body = json!({ "replace_original": true, "text": chosen.trim() });
            let request =
                self.http.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string());
            http::send(KIND, request, CALL_TIMEOUT, MAX_ANSWER_BYTES).await.map(|_| ())
        }
    }

    impl Channel for SlackBot {
        fn kind(&self) -> ChannelKind {
            KIND
        }

        fn send_text<'a>(
            &'a self,
            chat_id: &'a str,
            markdown: &'a str,
            buttons: &'a [Button],
        ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>> {
            Box::pin(self.send_parts(chat_id, markdown, buttons))
        }

        fn send_file<'a>(
            &'a self,
            chat_id: &'a str,
            file: &'a OutboundFile,
        ) -> BoxFuture<'a, Result<String, ChannelError>> {
            Box::pin(self.upload(chat_id, file))
        }

        /// Slack shows no typing sign for apps outside its AI assistant pane: a
        /// tap on a button is marked as taken instead.
        fn typing<'a>(&'a self, message: &'a InboundMessage) -> BoxFuture<'a, Result<(), ChannelError>> {
            Box::pin(async move {
                if message.choice.is_some() {
                    return self.mark_tapped(message).await;
                }
                Ok(())
            })
        }

        fn draws_cards(&self) -> bool {
            true
        }

        fn send_rich<'a>(
            &'a self,
            chat_id: &'a str,
            message: &'a RichMessage,
        ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>> {
            Box::pin(self.rich(chat_id, message))
        }

        fn download<'a>(
            &'a self,
            attachment: &'a Attachment,
            max_bytes: u64,
        ) -> BoxFuture<'a, Result<Download, ChannelError>> {
            Box::pin(self.fetch(attachment, max_bytes))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_token_must_be_a_bot_token_and_never_prints() {
            let http = crate::http::test_client();
            let bot = SlackBot::new(http.clone(), Secret::new("xoxb-1234-5678-abcdefgh"));
            assert!(bot.as_ref().is_ok_and(|bot| !format!("{bot:?}").contains("abcdefgh")));
            for bad in ["", "xoxp-1234-5678-abcdefgh", "xoxb-12", "xoxb-1234 5678-abcdefgh"] {
                assert!(
                    matches!(SlackBot::new(http.clone(), Secret::new(bad)), Err(ChannelError::Invalid(_))),
                    "{bad}"
                );
            }
        }

        #[test]
        fn the_token_only_goes_to_slack_file_host_or_the_api_server() {
            let bot = SlackBot::new(crate::http::test_client(), Secret::new("xoxb-1234-5678-abcdefgh"))
                .unwrap_or_else(|e| unreachable!("{e}"));
            assert!(bot.may_carry_token("https://files.slack.com/files-pri/T1-F1/download/a.pdf"));
            assert!(!bot.may_carry_token("https://files.slack.com.evil.example/x"));
            assert!(!bot.may_carry_token("https://example.com/x"));
            let local = bot.with_base_url("http://127.0.0.1:9/api");
            assert!(local.may_carry_token("http://127.0.0.1:9/files/a"));
            assert!(!local.may_carry_token("http://127.0.0.1:99/files/a"));
        }
    }
}

#[cfg(feature = "socket-mode")]
mod socket {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value as Json, json};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

    use super::{parse_event_callback, parse_interaction, text_of};
    use crate::http::{self, CALL_TIMEOUT, MAX_ANSWER_BYTES};
    use crate::model::{ChannelError, ChannelKind, InboundEvent, Secret};

    const KIND: ChannelKind = ChannelKind::Slack;
    /// Largest message read from the socket.
    const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
    /// Slack pings about every 30 seconds: a minute of silence is a dead connection.
    const READ_TIMEOUT: Duration = Duration::from_secs(90);

    /// A byte stream a WebSocket runs on: TLS, or plain TCP to a local test server.
    trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

    fn transport(reason: impl Into<String>) -> ChannelError {
        ChannelError::Transport { channel: KIND, reason: reason.into() }
    }

    /// Socket Mode of one app, through its app level token (`xapp-...`, scope
    /// `connections:write`).
    #[derive(Clone, Debug)]
    pub struct SocketMode {
        http: reqwest::Client,
        base: String,
        app_token: Secret,
    }

    /// What the socket delivered.
    #[derive(Debug)]
    pub enum SocketEvent {
        /// An event or a tap, already acknowledged to Slack.
        Event(InboundEvent),
        /// Slack asks the app to connect again (it rotates connections every few hours).
        Reconnect,
    }

    /// One open Socket Mode connection.
    pub struct SocketConnection {
        socket: WebSocketStream<Box<dyn Io>>,
    }

    impl std::fmt::Debug for SocketConnection {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SocketConnection")
        }
    }

    impl SocketMode {
        /// @throws ChannelError - `Invalid` when `app_token` is not an app level token.
        pub fn new(http: reqwest::Client, app_token: Secret) -> Result<Self, ChannelError> {
            let shaped = app_token.expose().starts_with("xapp-")
                && app_token.expose().len() >= 16
                && app_token.expose().bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !shaped {
                return Err(ChannelError::Invalid(
                    "the Slack app token is not one (expected xapp-..., from Basic Information > App-Level Tokens)"
                        .to_owned(),
                ));
            }
            Ok(Self { http, base: super::client::API_BASE.to_owned(), app_token })
        }

        /// Call another API server (a fake in tests).
        pub fn with_base_url(mut self, base: &str) -> Self {
            self.base = base.trim_end_matches('/').to_owned();
            self
        }

        /// Ask Slack for a WebSocket address (`apps.connections.open`).
        async fn open_url(&self) -> Result<String, ChannelError> {
            let request = self
                .http
                .post(format!("{}/apps.connections.open", self.base))
                .bearer_auth(self.app_token.expose())
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            let answer = http::send(KIND, request, CALL_TIMEOUT, MAX_ANSWER_BYTES).await?;
            let body = answer.json();
            if answer.status != 200 || body.get("ok").and_then(Json::as_bool) != Some(true) {
                return Err(ChannelError::Api {
                    channel: KIND,
                    method: "apps.connections.open".to_owned(),
                    status: answer.status,
                    code: None,
                    description: text_of(&body, "error").unwrap_or("no description").to_owned(),
                    retry_after: answer.retry_after,
                });
            }
            text_of(&body, "url").map(str::to_owned).ok_or_else(|| ChannelError::BadAnswer {
                channel: KIND,
                method: "apps.connections.open".to_owned(),
                reason: "no url".to_owned(),
            })
        }

        /// Open a connection.
        ///
        /// @throws ChannelError - When Slack refused the token or the socket did not open.
        pub async fn connect(&self) -> Result<SocketConnection, ChannelError> {
            let address = self.open_url().await?;
            let url = reqwest::Url::parse(&address).map_err(|_| transport("Slack gave an address that is not one"))?;
            let host = url.host_str().ok_or_else(|| transport("Slack gave an address without a host"))?.to_owned();
            let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]");
            let stream: Box<dyn Io> = match url.scheme() {
                "wss" => {
                    let port = url.port().unwrap_or(443);
                    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                        .await
                        .map_err(|_| transport("the connection failed"))?;
                    let config =
                        <rustls::ClientConfig as rustls_platform_verifier::ConfigVerifierExt>::with_platform_verifier()
                            .map_err(|_| transport("no TLS setup for the socket"))?;
                    let name = rustls::pki_types::ServerName::try_from(host.clone())
                        .map_err(|_| transport("Slack gave a host that is not one"))?;
                    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                        .connect(name, tcp)
                        .await
                        .map_err(|_| transport("the TLS handshake failed"))?;
                    Box::new(tls)
                }
                // Plain WebSocket only to this machine: a local Socket Mode server in tests.
                "ws" if loopback => {
                    let port = url.port().unwrap_or(80);
                    let tcp = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
                        .await
                        .map_err(|_| transport("the connection failed"))?;
                    Box::new(tcp)
                }
                _ => return Err(transport("Slack gave an address that is not wss")),
            };
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_FRAME_BYTES))
                .max_frame_size(Some(MAX_FRAME_BYTES));
            let (socket, _) = tokio_tungstenite::client_async_with_config(address.as_str(), stream, Some(config))
                .await
                .map_err(|_| transport("the WebSocket handshake failed"))?;
            Ok(SocketConnection { socket })
        }
    }

    impl SocketConnection {
        /// The next event, acknowledged to Slack before it is returned, so Slack
        /// does not deliver it again. `Ok(None)` when the connection closed.
        ///
        /// @throws ChannelError - `Transport` when the connection broke or went silent.
        pub async fn next(&mut self) -> Result<Option<SocketEvent>, ChannelError> {
            loop {
                let frame = match tokio::time::timeout(READ_TIMEOUT, self.socket.next()).await {
                    Err(_) => return Err(transport("the socket went silent")),
                    Ok(None) => return Ok(None),
                    Ok(Some(Err(_))) => return Err(transport("the socket broke")),
                    Ok(Some(Ok(frame))) => frame,
                };
                let text = match frame {
                    Message::Text(text) => text,
                    Message::Close(_) => return Ok(None),
                    // Pings are answered by the socket itself.
                    _ => continue,
                };
                let Ok(envelope) = serde_json::from_str::<Json>(text.as_str()) else { continue };
                if let Some(id) = text_of(&envelope, "envelope_id") {
                    let ack = json!({ "envelope_id": id }).to_string();
                    self.socket.send(Message::Text(ack.into())).await.map_err(|_| transport("the socket broke"))?;
                }
                let payload = envelope.get("payload").unwrap_or(&Json::Null);
                match text_of(&envelope, "type") {
                    Some("events_api") => return Ok(Some(SocketEvent::Event(parse_event_callback(payload)))),
                    Some("interactive") => return Ok(Some(SocketEvent::Event(parse_interaction(payload)))),
                    Some("disconnect") => return Ok(Some(SocketEvent::Reconnect)),
                    _ => {}
                }
            }
        }

        /// Close the connection.
        pub async fn close(mut self) {
            let _closed = self.socket.close(None).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";

    fn message(event: InboundEvent) -> InboundMessage {
        match event {
            InboundEvent::Message(message) => *message,
            other => unreachable!("expected a message, got {other:?}"),
        }
    }

    fn callback(event: Json) -> Json {
        json!({ "type": "event_callback", "team_id": "T1", "event_id": "Ev1", "event": event })
    }

    #[test]
    fn a_signature_is_checked_on_the_raw_body_and_its_time() {
        let secret = Secret::new(SECRET);
        let body = br#"{"type":"event_callback"}"#;
        let signature = sign(body, "1700000000", &secret);
        assert!(verify_signature(body, Some("1700000000"), Some(&signature), &secret, 1_700_000_100).is_ok());
        let tampered = br#"{"type":"event_callback "}"#;
        assert!(matches!(
            verify_signature(tampered, Some("1700000000"), Some(&signature), &secret, 1_700_000_100),
            Err(ChannelError::BadSignature(_))
        ));
        assert!(matches!(
            verify_signature(body, Some("1700000000"), Some(&signature), &secret, 1_700_000_000 + 301),
            Err(ChannelError::Stale)
        ));
        assert!(matches!(
            verify_signature(body, None, Some(&signature), &secret, 0),
            Err(ChannelError::MissingSignature(_))
        ));
        assert!(verify_signature(body, Some("1700000000"), Some("v0=zz"), &secret, 1_700_000_000).is_err());
        assert!(verify_signature(body, Some("1700000000"), Some(&signature), &Secret::new(""), 1_700_000_000).is_err());
    }

    #[test]
    fn the_address_check_returns_its_challenge() {
        let body = br#"{"type":"url_verification","challenge":"3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P","token":"x"}"#;
        assert_eq!(
            parse_delivery(body).ok(),
            Some(Delivery::Challenge("3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P".to_owned()))
        );
        assert!(parse_delivery(b"not json").is_err());
    }

    #[test]
    fn a_direct_message_is_private_and_a_thread_in_it_is_its_own_conversation() {
        let event =
            callback(json!({"type": "message", "channel": "D024BE91L", "channel_type": "im", "user": "U2147483697",
            "text": "Halo &amp; <https://shop.example|our shop>", "ts": "1355517523.000005"}));
        let parsed = message(parse_event_callback(&event));
        assert_eq!((parsed.chat_id.as_str(), parsed.chat_kind), ("D024BE91L", ChatKind::Private));
        assert_eq!(parsed.text, "Halo & our shop (https://shop.example)");
        assert_eq!(parsed.id, "D024BE91L:1355517523.000005");
        assert_eq!(parsed.timestamp_ms, Some(1_355_517_523_000));
        let threaded = callback(json!({"type": "message", "channel": "D024BE91L", "channel_type": "im", "user": "U1",
            "text": "more", "ts": "1355517600.000001", "thread_ts": "1355517523.000005"}));
        assert_eq!(message(parse_event_callback(&threaded)).chat_id, "D024BE91L:1355517523.000005");
    }

    #[test]
    fn a_mention_in_a_channel_starts_a_thread_and_plain_channel_messages_are_not_for_the_app() {
        let mention = callback(json!({"type": "app_mention", "channel": "C1", "user": "U1",
            "text": "<@U0LAN0Z89> what is our refund policy?", "ts": "1700000000.000100"}));
        let parsed = message(parse_event_callback(&mention));
        assert_eq!((parsed.chat_id.as_str(), parsed.chat_kind), ("C1:1700000000.000100", ChatKind::Group));
        assert_eq!(parsed.text, "what is our refund policy?");
        let in_thread = callback(json!({"type": "app_mention", "channel": "C1", "user": "U2",
            "text": "<@U0LAN0Z89> and for <@U3>?", "ts": "1700000500.000100", "thread_ts": "1700000000.000100"}));
        let parsed = message(parse_event_callback(&in_thread));
        assert_eq!(parsed.chat_id, "C1:1700000000.000100", "the thread is one conversation");
        assert_eq!(parsed.text, "and for @U3?");
        let plain = callback(json!({"type": "message", "channel": "C1", "channel_type": "channel", "user": "U1",
            "text": "lunch?", "ts": "1700000000.000200"}));
        assert!(matches!(parse_event_callback(&plain), InboundEvent::Ignored(_)));
    }

    #[test]
    fn bots_edits_and_other_events_are_ignored_and_files_are_kept() {
        for event in [
            json!({"type": "message", "channel": "D1", "channel_type": "im", "bot_id": "B1", "text": "hi", "ts": "1.1"}),
            json!({"type": "message", "subtype": "message_changed", "channel": "D1", "channel_type": "im", "ts": "1.1"}),
            json!({"type": "reaction_added", "user": "U1"}),
        ] {
            assert!(matches!(parse_event_callback(&callback(event)), InboundEvent::Ignored(_)));
        }
        let shared =
            callback(json!({"type": "message", "subtype": "file_share", "channel": "D1", "channel_type": "im",
            "user": "U1", "text": "", "ts": "1700000000.000300",
            "files": [{"id": "F1", "name": "faktur.pdf", "mimetype": "application/pdf", "size": 2048,
                       "url_private_download": "https://files.slack.com/files-pri/T1-F1/download/faktur.pdf"}]}));
        let parsed = message(parse_event_callback(&shared));
        let file = &parsed.attachments[0];
        assert_eq!(
            (file.kind, file.size, file.name.as_deref()),
            (AttachmentKind::Document, Some(2048), Some("faktur.pdf"))
        );
        assert!(file.file_id.starts_with("https://files.slack.com/"));
    }

    #[test]
    fn a_tap_on_a_button_is_a_choice_in_its_thread() {
        let payload = json!({"type": "block_actions", "user": {"id": "U7", "name": "rina"},
            "channel": {"id": "C1"}, "container": {"channel_id": "C1", "thread_ts": "1700000000.000100"},
            "message": {"ts": "1700000100.000100", "thread_ts": "1700000000.000100", "text": "Refund?"},
            "response_url": "https://hooks.slack.com/actions/T1/1/abc",
            "actions": [{"action_id": "graphrt_choice_0", "value": "1", "text": {"type": "plain_text", "text": "Approve"},
                         "action_ts": "1700000200.000001"}]});
        let encoded: String = payload
            .to_string()
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() { char::from(b).to_string() } else { format!("%{b:02X}") })
            .collect();
        let encoded = format!("payload={encoded}");
        let Ok(Delivery::Event(event)) = parse_delivery(encoded.as_bytes()) else { unreachable!("a tap") };
        let tap = message(event);
        assert_eq!(
            (tap.chat_id.as_str(), tap.choice.as_deref(), tap.text.as_str()),
            ("C1:1700000000.000100", Some("1"), "Approve")
        );
        assert_eq!(tap.customer.id, "U7");
        assert_eq!(tap.reference.as_deref(), Some("https://hooks.slack.com/actions/T1/1/abc\nRefund?"));
        let mut other = payload.clone();
        other["actions"][0]["action_id"] = json!("someone_else");
        assert!(matches!(parse_interaction(&other), InboundEvent::Ignored(_)));
        let mut elsewhere = payload;
        elsewhere["response_url"] = json!("https://evil.example/x");
        assert_eq!(message(parse_interaction(&elsewhere)).reference, None, "taps are only marked on Slack's own host");
    }

    #[test]
    fn a_card_becomes_block_kit_when_block_kit_can_show_it() {
        let card = json!({"type": "AdaptiveCard", "version": "1.5", "body": [
            {"type": "TextBlock", "text": "Refund request", "size": "large"},
            {"type": "TextBlock", "text": "Order **#1042** for <Sari>"},
            {"type": "FactSet", "facts": [{"title": "Amount", "value": "Rp 250.000"}]},
            {"type": "Image", "url": "https://cdn.example/r.png", "altText": "receipt"},
            {"type": "Image", "url": "data:image/png;base64,AAAA"}],
            "actions": [{"type": "Action.Submit", "title": "Approve"}, {"type": "Action.Submit", "title": "Reject"},
                        {"type": "Action.OpenUrl", "title": "Policy", "url": "https://shop.example/p"}]});
        let choices =
            [Button { id: "1".into(), title: "Approve".into() }, Button { id: "2".into(), title: "Reject".into() }];
        let blocks = card_blocks(&card, &choices).unwrap_or_default();
        let kinds: Vec<&str> = blocks.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, ["header", "section", "section", "image", "actions"]);
        assert_eq!(blocks[1]["text"]["text"], "Order *#1042* for &lt;Sari&gt;");
        assert_eq!(blocks[2]["fields"][0]["text"], "*Amount*\nRp 250.000");
        let buttons = blocks[4]["elements"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            (buttons[0]["value"].as_str(), buttons[1]["action_id"].as_str()),
            (Some("1"), Some("graphrt_choice_1"))
        );
        assert_eq!(buttons[2]["url"], "https://shop.example/p");

        let form = json!({"type": "AdaptiveCard", "body": [{"type": "Input.Text", "id": "note"}],
            "actions": [{"type": "Action.Submit", "title": "Send"}]});
        assert_eq!(card_blocks(&form, &choices[..1]), None, "a text field has no Block Kit answer here");
        let shows =
            json!({"type": "AdaptiveCard", "body": [], "actions": [{"type": "Action.ShowCard", "title": "More"}]});
        assert_eq!(card_blocks(&shows, &[]), None);
    }

    #[test]
    fn too_many_buttons_stay_in_the_text() {
        let many: Vec<Button> = (1..=26).map(|n| Button { id: n.to_string(), title: format!("Option {n}") }).collect();
        assert!(buttons_block(&many, &[]).is_none());
        assert!(buttons_block(&many[..3], &[]).is_some());
        assert!(buttons_block(&[], &[]).is_none());
    }

    #[test]
    fn chats_split_into_channel_and_thread() {
        assert_eq!(split_chat("C1:1700000000.000100"), ("C1", Some("1700000000.000100")));
        assert_eq!(split_chat("D1"), ("D1", None));
        assert_eq!(split_chat("U1"), ("U1", None));
    }
}
