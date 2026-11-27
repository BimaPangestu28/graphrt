//! WhatsApp, through the official [Cloud API](https://developers.facebook.com/docs/whatsapp/cloud-api)
//! of the WhatsApp Business Platform.
//!
//! WhatsApp only delivers by webhook, so a public HTTPS address is needed:
//!
//! 1. When the address is registered, Meta sends a `GET` with `hub.mode`,
//!    `hub.verify_token` and `hub.challenge`. [`verify_subscription`] answers it.
//! 2. Every delivery is a `POST` signed with the app secret in the header
//!    [`SIGNATURE_HEADER`]. Check the raw body with [`verify_signature`] before
//!    parsing it, then read it with [`parse_webhook`].
//! 3. Answer `200` at once. Meta delivers again, for days, what it gets no `200` for.
//!
//! One delivery can hold several messages and delivery reports, of several
//! phone numbers of the same business account.
//!
//! **The 24 hour customer service window.** A business may write free text to a
//! customer only within 24 hours of the customer's last message. After that only
//! an approved template message is accepted, and it is billed. [`service_window_open`]
//! tells which case applies. Sending free text outside the window fails with
//! [`crate::ChannelError::OutsideServiceWindow`].
//!
//! Groups are not read: this module handles chats between one customer and the
//! business number.

use serde_json::Value as Json;

use crate::model::{
    Attachment, AttachmentKind, ChannelError, ChatKind, Customer, DeliveryState, DeliveryStatus, InboundEvent,
    InboundMessage, Secret,
};
use crate::sign::{constant_time_eq, from_hex, hmac_sha256};

/// The header that carries a delivery's signature: `sha256=<hex>`.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";

/// How long after a customer's last message free text may be sent.
pub const SERVICE_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// Whether free text may be sent to a customer whose last message arrived at
/// `last_inbound_ms`. A minute is kept back: a message that leaves at the edge
/// arrives after it.
pub fn service_window_open(last_inbound_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_inbound_ms) < SERVICE_WINDOW_MS - 60_000
}

/// Answer Meta's check of a webhook address: the value of `hub.challenge`, to
/// send back as the body of a `200`, when `hub.mode` is `subscribe` and
/// `hub.verify_token` is the configured token.
///
/// @throws ChannelError - `MissingSignature` or `BadSignature`: answer `403`.
pub fn verify_subscription(
    mode: Option<&str>,
    token: Option<&str>,
    challenge: Option<&str>,
    verify_token: &Secret,
) -> Result<String, ChannelError> {
    let (Some(token), Some(challenge)) = (token, challenge) else {
        return Err(ChannelError::MissingSignature("verify token"));
    };
    let matches = !verify_token.is_empty() && constant_time_eq(token.as_bytes(), verify_token.expose().as_bytes());
    if mode != Some("subscribe") || !matches {
        return Err(ChannelError::BadSignature("verify token"));
    }
    Ok(challenge.to_owned())
}

/// Check that `body` (the raw bytes, before any parsing) was signed with the
/// app secret. `header` is the value of [`SIGNATURE_HEADER`].
///
/// @throws ChannelError - `MissingSignature` without the header, `BadSignature` otherwise: answer `401`.
pub fn verify_signature(body: &[u8], header: Option<&str>, app_secret: &Secret) -> Result<(), ChannelError> {
    let header = header.ok_or(ChannelError::MissingSignature("signature"))?;
    let given = header.trim().strip_prefix("sha256=").and_then(from_hex);
    let Some(given) = given else {
        return Err(ChannelError::BadSignature("signature"));
    };
    let expected = hmac_sha256(app_secret.expose().as_bytes(), body);
    if app_secret.is_empty() || !constant_time_eq(&given, &expected) {
        return Err(ChannelError::BadSignature("signature"));
    }
    Ok(())
}

/// The signature header value of `body` under `app_secret`: what Meta sends.
/// For tests and for tools that replay a delivery.
pub fn sign(body: &[u8], app_secret: &Secret) -> String {
    format!("sha256={}", crate::sign::to_hex(&hmac_sha256(app_secret.expose().as_bytes(), body)))
}

/// One thing a delivery holds, and the business phone number it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhatsAppEvent {
    /// The id of the business phone number (not the number itself).
    pub phone_number_id: String,
    pub event: InboundEvent,
}

/// Read a verified delivery. Something that is not a WhatsApp delivery gives no events.
pub fn parse_webhook(payload: &Json) -> Vec<WhatsAppEvent> {
    let mut events = Vec::new();
    if payload.get("object").and_then(Json::as_str) != Some("whatsapp_business_account") {
        return events;
    }
    let list = |value: &Json, key: &str| value.get(key).and_then(Json::as_array).cloned().unwrap_or_default();
    for entry in list(payload, "entry") {
        for change in list(&entry, "changes") {
            if change.get("field").and_then(Json::as_str) != Some("messages") {
                continue;
            }
            let Some(value) = change.get("value") else { continue };
            let phone_number_id = value
                .get("metadata")
                .and_then(|metadata| metadata.get("phone_number_id"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            if phone_number_id.is_empty() {
                continue;
            }
            let contacts = list(value, "contacts");
            let name_of = |wa_id: &str| {
                contacts
                    .iter()
                    .find(|contact| contact.get("wa_id").and_then(Json::as_str) == Some(wa_id))
                    .and_then(|contact| contact.get("profile"))
                    .and_then(|profile| text_of(profile, "name"))
                    .map(str::to_owned)
            };
            let events_of_value = list(value, "messages")
                .iter()
                .map(|message| message_event(message, &name_of))
                .chain(list(value, "statuses").iter().map(status_event))
                .collect::<Vec<_>>();
            for event in events_of_value {
                events.push(WhatsAppEvent { phone_number_id: phone_number_id.to_owned(), event });
            }
        }
    }
    events
}

fn text_of<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).filter(|text| !text.is_empty())
}

/// Seconds since 1970, sent as a string.
fn timestamp_ms(value: &Json) -> Option<u64> {
    let seconds = match value.get("timestamp")? {
        Json::String(text) => text.parse::<u64>().ok()?,
        Json::Number(number) => number.as_u64()?,
        _ => return None,
    };
    Some(seconds.saturating_mul(1000))
}

fn message_event(message: &Json, name_of: &dyn Fn(&str) -> Option<String>) -> InboundEvent {
    let (Some(from), Some(id)) = (text_of(message, "from"), text_of(message, "id")) else {
        return InboundEvent::Ignored("a message without ids");
    };
    // A phone number, digits only: it becomes part of ids and file paths.
    if !from.bytes().all(|b| b.is_ascii_digit()) || from.len() > 20 {
        return InboundEvent::Ignored("a sender that is not a phone number");
    }
    let kind = text_of(message, "type").unwrap_or("unknown");
    let part = message.get(kind);
    let mut text = String::new();
    let mut attachments = Vec::new();
    let mut choice = None;
    let mut unsupported = None;
    match kind {
        "text" => text = part.and_then(|part| text_of(part, "body")).unwrap_or_default().to_owned(),
        "image" | "document" | "audio" | "video" => {
            let media = part.unwrap_or(&Json::Null);
            text = text_of(media, "caption").unwrap_or_default().to_owned();
            if let Some(media_id) = text_of(media, "id") {
                attachments.push(Attachment {
                    file_id: media_id.to_owned(),
                    kind: match kind {
                        "image" => AttachmentKind::Image,
                        "audio" => AttachmentKind::Audio,
                        "video" => AttachmentKind::Video,
                        _ => AttachmentKind::Document,
                    },
                    name: text_of(media, "filename").map(str::to_owned),
                    // `audio/ogg; codecs=opus`: the type is the part before the parameters.
                    mime_type: text_of(media, "mime_type")
                        .map(|mime| mime.split(';').next().unwrap_or(mime).trim().to_owned()),
                    size: None,
                });
            }
        }
        "interactive" => {
            let reply = part.and_then(|part| part.get("button_reply").or_else(|| part.get("list_reply")));
            match reply.and_then(|reply| text_of(reply, "id")) {
                Some(reply_id) => {
                    choice = Some(reply_id.to_owned());
                    text = reply.and_then(|reply| text_of(reply, "title")).unwrap_or(reply_id).to_owned();
                }
                None => unsupported = Some("interactive".to_owned()),
            }
        }
        // A tap on a quick reply button of a template message.
        "button" => {
            text = part.and_then(|part| text_of(part, "text")).unwrap_or_default().to_owned();
            choice = part.and_then(|part| text_of(part, "payload")).map(str::to_owned);
        }
        "reaction" => return InboundEvent::Ignored("a reaction"),
        "system" => return InboundEvent::Ignored("a system message"),
        other => unsupported = Some(other.to_owned()),
    }
    InboundEvent::Message(Box::new(InboundMessage {
        id: id.to_owned(),
        chat_id: from.to_owned(),
        chat_kind: ChatKind::Private,
        customer: Customer { id: from.to_owned(), name: name_of(from) },
        text,
        attachments,
        choice,
        reference: Some(id.to_owned()),
        timestamp_ms: timestamp_ms(message),
        unsupported,
    }))
}

fn status_event(status: &Json) -> InboundEvent {
    let state = match text_of(status, "status") {
        Some("sent") => DeliveryState::Sent,
        Some("delivered") => DeliveryState::Delivered,
        Some("read") => DeliveryState::Read,
        Some("failed") => DeliveryState::Failed,
        _ => return InboundEvent::Ignored("a status that is not a delivery report"),
    };
    let Some(message_id) = text_of(status, "id") else {
        return InboundEvent::Ignored("a status without a message id");
    };
    let error = status.get("errors").and_then(Json::as_array).and_then(|errors| errors.first()).map(|error| {
        let code = error.get("code").and_then(Json::as_i64).unwrap_or_default();
        let title = text_of(error, "title").or_else(|| text_of(error, "message")).unwrap_or("no description");
        format!("{code}: {title}")
    });
    InboundEvent::Status(DeliveryStatus {
        message_id: message_id.to_owned(),
        recipient: text_of(status, "recipient_id").unwrap_or_default().to_owned(),
        state,
        error,
    })
}

#[cfg(feature = "client")]
pub use client::{MediaInfo, WhatsAppCloud};

#[cfg(feature = "client")]
mod client {
    use std::time::Duration;

    use serde_json::{Value as Json, json};

    use super::text_of;
    use crate::channel::{BoxFuture, Channel, Download, OutboundFile};
    use crate::http::{self, Answer, CALL_TIMEOUT, MAX_ANSWER_BYTES, Multipart, TRANSFER_TIMEOUT, safe_file_name};
    use crate::model::{Attachment, Button, ChannelError, ChannelKind, InboundMessage, Secret};
    use crate::text;

    /// Where the Graph API lives.
    pub const API_BASE: &str = "https://graph.facebook.com";
    /// The Graph API version called unless another is set.
    pub const API_VERSION: &str = "v23.0";
    /// Source text per message (WhatsApp takes 4096 characters).
    const TEXT_UNITS: usize = 4000;
    const CAPTION_UNITS: usize = 1000;
    /// What reply buttons allow: three of them, 20 characters each, under at most 1024 characters.
    const MAX_BUTTONS: usize = 3;
    const MAX_BUTTON_TITLE_UNITS: usize = 20;
    const MAX_BUTTON_BODY_UNITS: usize = 1024;
    const MAX_BUTTON_ID_BYTES: usize = 256;
    /// Largest picture and largest document WhatsApp takes.
    const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    const MAX_DOCUMENT_BYTES: usize = 100 * 1024 * 1024;
    /// Error code of free text sent outside the customer service window.
    const CODE_OUTSIDE_WINDOW: i64 = 131_047;
    /// Error codes that mean "slow down", sent with a `400`.
    const RATE_LIMIT_CODES: [i64; 5] = [4, 80_007, 130_429, 131_048, 131_056];
    const KIND: ChannelKind = ChannelKind::WhatsApp;

    /// Where a file a customer sent can be fetched, and what it is.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct MediaInfo {
        /// Valid for five minutes, and only with the access token.
        pub url: String,
        pub mime_type: Option<String>,
        pub size: Option<u64>,
    }

    /// One WhatsApp business phone number.
    #[derive(Clone, Debug)]
    pub struct WhatsAppCloud {
        http: reqwest::Client,
        base: String,
        version: String,
        phone_number_id: String,
        token: Secret,
    }

    impl WhatsAppCloud {
        /// The business number with id `phone_number_id` (from the app's
        /// WhatsApp settings, not the phone number itself), calling through
        /// `http` with `token`.
        ///
        /// @throws ChannelError - `Invalid` when the id is not a number, or the token is empty.
        pub fn new(http: reqwest::Client, phone_number_id: &str, token: Secret) -> Result<Self, ChannelError> {
            // The id is a path segment of every request.
            if phone_number_id.is_empty() || !phone_number_id.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ChannelError::Invalid(
                    "the WhatsApp phone number id is a number (from the app's WhatsApp settings, not the phone number)"
                        .to_owned(),
                ));
            }
            if token.is_empty() {
                return Err(ChannelError::Invalid("the WhatsApp access token is empty".to_owned()));
            }
            Ok(Self {
                http,
                base: API_BASE.to_owned(),
                version: API_VERSION.to_owned(),
                phone_number_id: phone_number_id.to_owned(),
                token,
            })
        }

        /// Call another server instead of `graph.facebook.com` (a fake in tests).
        pub fn with_base_url(mut self, base: &str) -> Self {
            self.base = base.trim_end_matches('/').to_owned();
            self
        }

        /// Call another Graph API version, e.g. `v24.0`.
        ///
        /// @throws ChannelError - `Invalid` when `version` is not of that form.
        pub fn with_api_version(mut self, version: &str) -> Result<Self, ChannelError> {
            let shaped = version
                .strip_prefix('v')
                .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
            if !shaped {
                return Err(ChannelError::Invalid(format!("`{version}` is not a Graph API version such as v23.0")));
            }
            self.version = version.to_owned();
            Ok(self)
        }

        /// The id of the business phone number.
        pub fn phone_number_id(&self) -> &str {
            &self.phone_number_id
        }

        fn refusal(&self, method: &str, answer: &Answer) -> ChannelError {
            let body = answer.json();
            let error = body.get("error").unwrap_or(&Json::Null);
            let code = error.get("code").and_then(Json::as_i64);
            if code == Some(CODE_OUTSIDE_WINDOW) {
                return ChannelError::OutsideServiceWindow;
            }
            let details = error.get("error_data").and_then(|data| text_of(data, "details"));
            let description = details.or_else(|| text_of(error, "message")).unwrap_or("no description");
            // "Slow down" arrives as a `400` with its own code: report it as what it is.
            let slow_down = code.is_some_and(|code| RATE_LIMIT_CODES.contains(&code));
            ChannelError::Api {
                channel: KIND,
                method: method.to_owned(),
                status: if slow_down { 429 } else { answer.status },
                code,
                description: self.token.redact(&text::truncate(description, 300)),
                retry_after: None,
            }
        }

        async fn request(
            &self,
            method: &str,
            request: reqwest::RequestBuilder,
            timeout: Duration,
        ) -> Result<Json, ChannelError> {
            let request = request.bearer_auth(self.token.expose());
            let answer = http::send(KIND, request, timeout, MAX_ANSWER_BYTES).await?;
            if (200..300).contains(&answer.status) {
                return Ok(answer.json());
            }
            Err(self.refusal(method, &answer))
        }

        /// `POST /{phone-number-id}/messages`.
        async fn post_message(&self, body: &Json) -> Result<Json, ChannelError> {
            let url = format!("{}/{}/{}/messages", self.base, self.version, self.phone_number_id);
            let bytes = serde_json::to_vec(body).map_err(|e| ChannelError::Invalid(e.to_string()))?;
            let request = self.http.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(bytes);
            self.request("messages", request, CALL_TIMEOUT).await
        }

        /// The id of a message the API accepted.
        fn sent_id(answer: &Json) -> Result<String, ChannelError> {
            answer
                .get("messages")
                .and_then(Json::as_array)
                .and_then(|messages| messages.first())
                .and_then(|message| text_of(message, "id"))
                .map(str::to_owned)
                .ok_or_else(|| ChannelError::BadAnswer {
                    channel: KIND,
                    method: "messages".to_owned(),
                    reason: "no message id".to_owned(),
                })
        }

        fn envelope(to: &str, kind: &str, content: Json) -> Json {
            let mut body =
                json!({ "messaging_product": "whatsapp", "recipient_type": "individual", "to": to, "type": kind });
            if let Some(object) = body.as_object_mut() {
                object.insert(kind.to_owned(), content);
            }
            body
        }

        /// Send one text message of at most 4096 characters, in WhatsApp's own
        /// formatting. Only inside the customer service window.
        ///
        /// @returns The message id.
        /// @throws ChannelError - `OutsideServiceWindow` when the customer's last message is older than 24 hours.
        pub async fn send_text_message(&self, to: &str, body: &str) -> Result<String, ChannelError> {
            let answer = self.post_message(&Self::envelope(to, "text", json!({ "body": body }))).await?;
            Self::sent_id(&answer)
        }

        /// Send a text with up to three reply buttons under it.
        ///
        /// @throws ChannelError - `Invalid` when WhatsApp cannot show these as buttons (see [`fits_buttons`]).
        pub async fn send_buttons(&self, to: &str, body: &str, buttons: &[Button]) -> Result<String, ChannelError> {
            if !fits_buttons(body, buttons) {
                return Err(ChannelError::Invalid(
                    "WhatsApp shows at most 3 reply buttons of 20 characters under at most 1024 characters".to_owned(),
                ));
            }
            let replies: Vec<Json> = buttons
                .iter()
                .map(|button| json!({ "type": "reply", "reply": { "id": button.id, "title": button.title } }))
                .collect();
            let content = json!({ "type": "button", "body": { "text": body }, "action": { "buttons": replies } });
            Self::sent_id(&self.post_message(&Self::envelope(to, "interactive", content)).await?)
        }

        /// Mark the customer's message `message_id` as read (two blue ticks),
        /// and with `typing` show that an answer is being written, for up to 25
        /// seconds or until the next message is sent.
        pub async fn mark_read(&self, message_id: &str, typing: bool) -> Result<(), ChannelError> {
            let mut body = json!({ "messaging_product": "whatsapp", "status": "read", "message_id": message_id });
            if let (true, Some(object)) = (typing, body.as_object_mut()) {
                object.insert("typing_indicator".to_owned(), json!({ "type": "text" }));
            }
            self.post_message(&body).await.map(|_| ())
        }

        /// Send the approved template `name` in `language` (e.g. `en_US`), with
        /// `body_parameters` for its `{{1}}`, `{{2}}`... The one message WhatsApp
        /// takes outside the customer service window. Meta bills it.
        pub async fn send_template(
            &self,
            to: &str,
            name: &str,
            language: &str,
            body_parameters: &[String],
        ) -> Result<String, ChannelError> {
            let mut template = json!({ "name": name, "language": { "code": language } });
            if let (false, Some(object)) = (body_parameters.is_empty(), template.as_object_mut()) {
                let parameters: Vec<Json> =
                    body_parameters.iter().map(|value| json!({ "type": "text", "text": value })).collect();
                object.insert("components".to_owned(), json!([{ "type": "body", "parameters": parameters }]));
            }
            Self::sent_id(&self.post_message(&Self::envelope(to, "template", template)).await?)
        }

        /// Upload a file to WhatsApp, to send it by the id this returns.
        pub async fn upload_media(&self, file: &OutboundFile) -> Result<String, ChannelError> {
            let (content_type, body) = Multipart::new()
                .text("messaging_product", "whatsapp")
                .text("type", &file.mime_type)
                .file("file", &safe_file_name(&file.name), &file.mime_type, &file.bytes)
                .finish();
            let url = format!("{}/{}/{}/media", self.base, self.version, self.phone_number_id);
            let request = self.http.post(url).header(reqwest::header::CONTENT_TYPE, content_type).body(body);
            let answer = self.request("media", request, TRANSFER_TIMEOUT).await?;
            text_of(&answer, "id").map(str::to_owned).ok_or_else(|| ChannelError::BadAnswer {
                channel: KIND,
                method: "media".to_owned(),
                reason: "no media id".to_owned(),
            })
        }

        /// Where the file `media_id` a customer sent can be fetched.
        pub async fn media(&self, media_id: &str) -> Result<MediaInfo, ChannelError> {
            // The id comes from a delivery: it is a path segment here.
            if media_id.is_empty() || !media_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(ChannelError::Invalid("a media id that is not one".to_owned()));
            }
            let url = format!("{}/{}/{media_id}", self.base, self.version);
            let info = self.request("media info", self.http.get(url), CALL_TIMEOUT).await?;
            let url = text_of(&info, "url").ok_or_else(|| ChannelError::BadAnswer {
                channel: KIND,
                method: "media info".to_owned(),
                reason: "no url".to_owned(),
            })?;
            Ok(MediaInfo {
                url: url.to_owned(),
                mime_type: text_of(&info, "mime_type").map(str::to_owned),
                size: info.get("file_size").and_then(Json::as_u64),
            })
        }

        async fn fetch(&self, attachment: &Attachment, max_bytes: u64) -> Result<Download, ChannelError> {
            let info = self.media(&attachment.file_id).await?;
            if info.size.is_some_and(|size| size > max_bytes) {
                return Err(ChannelError::TooLarge { limit: max_bytes });
            }
            // The access token goes with this request: only to HTTPS, or to the
            // configured API server itself (a fake in tests).
            if !info.url.starts_with("https://") && !info.url.starts_with(&format!("{}/", self.base)) {
                return Err(ChannelError::BadAnswer {
                    channel: KIND,
                    method: "media info".to_owned(),
                    reason: "a media address that is not HTTPS".to_owned(),
                });
            }
            let request = self.http.get(&info.url).bearer_auth(self.token.expose());
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, max_bytes).await?;
            if answer.status != 200 {
                return Err(ChannelError::Api {
                    channel: KIND,
                    method: "media download".to_owned(),
                    status: answer.status,
                    code: None,
                    description: "the file could not be downloaded".to_owned(),
                    retry_after: None,
                });
            }
            let mime_type = info.mime_type.or(answer.content_type).or_else(|| attachment.mime_type.clone());
            Ok(Download { bytes: answer.body, mime_type, name: attachment.name.clone() })
        }

        async fn send_parts(&self, to: &str, markdown: &str, buttons: &[Button]) -> Result<Vec<String>, ChannelError> {
            let parts = text::split(markdown, TEXT_UNITS);
            let mut sent = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                let body = text::to_whatsapp(part);
                let last = index + 1 == parts.len();
                let id = if last && fits_buttons(&body, buttons) {
                    self.send_buttons(to, &body, buttons).await?
                } else {
                    self.send_text_message(to, &body).await?
                };
                sent.push(id);
            }
            Ok(sent)
        }

        async fn upload_and_send(&self, to: &str, file: &OutboundFile) -> Result<String, ChannelError> {
            let as_image = file.is_image() && file.bytes.len() <= MAX_IMAGE_BYTES;
            if file.bytes.len() > MAX_DOCUMENT_BYTES {
                return Err(ChannelError::TooLarge { limit: u64::try_from(MAX_DOCUMENT_BYTES).unwrap_or(u64::MAX) });
            }
            let media_id = self.upload_media(file).await?;
            let mut content = json!({ "id": media_id });
            if let Some(object) = content.as_object_mut() {
                if let Some(caption) = file.caption.as_deref().filter(|caption| !caption.trim().is_empty()) {
                    let caption = text::to_whatsapp(&text::truncate(caption.trim(), CAPTION_UNITS));
                    object.insert("caption".to_owned(), json!(caption));
                }
                if !as_image {
                    object.insert("filename".to_owned(), json!(safe_file_name(&file.name)));
                }
            }
            let kind = if as_image { "image" } else { "document" };
            Self::sent_id(&self.post_message(&Self::envelope(to, kind, content)).await?)
        }
    }

    /// Whether WhatsApp can show `buttons` as reply buttons under `body`.
    pub(super) fn fits_buttons(body: &str, buttons: &[Button]) -> bool {
        (1..=MAX_BUTTONS).contains(&buttons.len())
            && !body.is_empty()
            && text::units(body) <= MAX_BUTTON_BODY_UNITS
            && buttons.iter().all(|button| {
                !button.id.is_empty()
                    && button.id.len() <= MAX_BUTTON_ID_BYTES
                    && !button.title.is_empty()
                    && text::units(&button.title) <= MAX_BUTTON_TITLE_UNITS
            })
    }

    impl Channel for WhatsAppCloud {
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
            Box::pin(self.upload_and_send(chat_id, file))
        }

        fn typing<'a>(&'a self, message: &'a InboundMessage) -> BoxFuture<'a, Result<(), ChannelError>> {
            Box::pin(async move {
                let Some(message_id) = message.reference.as_deref() else { return Ok(()) };
                match self.mark_read(message_id, true).await {
                    // An API version without the typing sign still marks as read.
                    Err(ChannelError::Api { status: 400, .. }) => self.mark_read(message_id, false).await,
                    other => other,
                }
            })
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

        fn cloud() -> WhatsAppCloud {
            WhatsAppCloud::new(crate::http::test_client(), "106540352242922", Secret::new("EAAG-access-token-value"))
                .unwrap_or_else(|e| unreachable!("{e}"))
        }

        #[test]
        fn the_account_must_be_named_by_its_id() {
            let http = crate::http::test_client();
            for bad in ["", "+62 812", "1065/../x"] {
                let refused = WhatsAppCloud::new(http.clone(), bad, Secret::new("token-value"));
                assert!(matches!(refused, Err(ChannelError::Invalid(_))), "{bad}");
            }
            assert!(WhatsAppCloud::new(http, "1065", Secret::new("")).is_err());
            assert!(cloud().with_api_version("v24.0").is_ok());
            assert!(cloud().with_api_version("../v1").is_err());
        }

        #[test]
        fn refusals_name_the_window_and_slow_downs_and_never_the_token() {
            let answer = |body: &str| Answer {
                status: 400,
                content_type: None,
                retry_after: None,
                body: body.as_bytes().to_vec(),
            };
            let outside = answer(r#"{"error":{"message":"(#131047) Re-engagement message","code":131047}}"#);
            assert!(matches!(cloud().refusal("messages", &outside), ChannelError::OutsideServiceWindow));
            let slow = cloud().refusal("messages", &answer(r#"{"error":{"message":"Rate limit hit","code":130429}}"#));
            assert!(slow.is_temporary(), "{slow}");
            let leaked = answer(r#"{"error":{"message":"bad token EAAG-access-token-value","code":190}}"#);
            let text = cloud().refusal("messages", &leaked).to_string();
            assert!(!text.contains("EAAG-access-token-value"), "{text}");
            assert!(!format!("{:?}", cloud()).contains("EAAG-access-token-value"));
        }

        #[test]
        fn reply_buttons_need_to_fit_whatsapps_limits() {
            let button = |title: &str| Button { id: "1".to_owned(), title: title.to_owned() };
            assert!(fits_buttons("Approve the refund?", &[button("Approve"), button("Reject")]));
            assert!(!fits_buttons("Pick", &[button("a"), button("b"), button("c"), button("d")]));
            assert!(!fits_buttons("Pick", &[button("a title that is far too long")]));
            assert!(!fits_buttons(&"x".repeat(1025), &[button("ok")]));
            assert!(!fits_buttons("Pick", &[]));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn delivery(value: Json) -> Json {
        let mut value = value;
        if let Some(object) = value.as_object_mut() {
            object.insert("messaging_product".into(), json!("whatsapp"));
            object.insert("metadata".into(), json!({"display_phone_number": "15550001111", "phone_number_id": "1065"}));
        }
        json!({"object": "whatsapp_business_account",
               "entry": [{"id": "biz", "changes": [{"field": "messages", "value": value}]}]})
    }

    fn only_message(payload: &Json) -> InboundMessage {
        let mut events = parse_webhook(payload);
        assert_eq!(events.len(), 1, "{events:?}");
        match events.remove(0).event {
            InboundEvent::Message(message) => *message,
            other => unreachable!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn a_text_message_names_its_sender_and_account() {
        let payload = delivery(json!({
            "contacts": [{"profile": {"name": "Budi"}, "wa_id": "6281234567890"}],
            "messages": [{"from": "6281234567890", "id": "wamid.A1", "timestamp": "1700000000",
                          "type": "text", "text": {"body": "Halo"}}],
        }));
        let events = parse_webhook(&payload);
        assert_eq!(events[0].phone_number_id, "1065");
        let message = only_message(&payload);
        assert_eq!((message.id.as_str(), message.chat_id.as_str()), ("wamid.A1", "6281234567890"));
        assert_eq!(message.customer, Customer { id: "6281234567890".into(), name: Some("Budi".into()) });
        assert_eq!(message.text, "Halo");
        assert_eq!(message.reference.as_deref(), Some("wamid.A1"));
        assert_eq!(message.timestamp_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn media_comes_with_its_caption_and_type() {
        let image = delivery(json!({"messages": [{"from": "62811", "id": "wamid.B", "timestamp": "1", "type": "image",
            "image": {"id": "9001", "mime_type": "image/jpeg", "sha256": "x", "caption": "the receipt"}}]}));
        let message = only_message(&image);
        assert_eq!(message.text, "the receipt");
        assert_eq!(message.attachments[0].file_id, "9001");
        assert_eq!(message.attachments[0].kind, AttachmentKind::Image);
        let voice = delivery(json!({"messages": [{"from": "62811", "id": "wamid.C", "timestamp": "1", "type": "audio",
            "audio": {"id": "9002", "mime_type": "audio/ogg; codecs=opus", "voice": true}}]}));
        assert_eq!(only_message(&voice).attachments[0].mime_type.as_deref(), Some("audio/ogg"));
        let document = delivery(json!({"messages": [{"from": "62811", "id": "wamid.D", "timestamp": "1",
            "type": "document", "document": {"id": "9003", "mime_type": "application/pdf", "filename": "a.pdf"}}]}));
        assert_eq!(only_message(&document).attachments[0].name.as_deref(), Some("a.pdf"));
    }

    #[test]
    fn a_tapped_button_is_a_message_with_a_choice() {
        let tap = delivery(json!({"messages": [{"from": "62811", "id": "wamid.E", "timestamp": "1",
            "type": "interactive", "interactive": {"type": "button_reply", "button_reply": {"id": "2", "title": "Reject"}}}]}));
        let message = only_message(&tap);
        assert_eq!((message.choice.as_deref(), message.text.as_str()), (Some("2"), "Reject"));
        let quick = delivery(json!({"messages": [{"from": "62811", "id": "wamid.F", "timestamp": "1",
            "type": "button", "button": {"text": "Yes", "payload": "yes-payload"}}]}));
        assert_eq!(only_message(&quick).choice.as_deref(), Some("yes-payload"));
    }

    #[test]
    fn statuses_are_delivery_reports_with_the_failure() {
        let payload = delivery(json!({"statuses": [
            {"id": "wamid.S1", "status": "delivered", "timestamp": "1", "recipient_id": "62811"},
            {"id": "wamid.S2", "status": "failed", "timestamp": "2", "recipient_id": "62811",
             "errors": [{"code": 131047, "title": "Re-engagement message"}]},
        ]}));
        let events = parse_webhook(&payload);
        assert_eq!(events.len(), 2);
        let InboundEvent::Status(failed) = &events[1].event else { unreachable!() };
        assert_eq!(failed.state, DeliveryState::Failed);
        assert_eq!(failed.error.as_deref(), Some("131047: Re-engagement message"));
    }

    #[test]
    fn what_an_agent_cannot_read_is_named_and_the_rest_ignored() {
        let location = delivery(json!({"messages": [{"from": "62811", "id": "wamid.G", "timestamp": "1",
            "type": "location", "location": {"latitude": 1.0, "longitude": 2.0}}]}));
        assert_eq!(only_message(&location).unsupported.as_deref(), Some("location"));
        let reaction = delivery(json!({"messages": [{"from": "62811", "id": "wamid.H", "timestamp": "1",
            "type": "reaction", "reaction": {"message_id": "wamid.A1", "emoji": "x"}}]}));
        assert!(matches!(parse_webhook(&reaction)[0].event, InboundEvent::Ignored(_)));
        let odd_sender = delivery(json!({"messages": [{"from": "../x", "id": "wamid.I", "type": "text",
            "text": {"body": "hi"}}]}));
        assert!(matches!(parse_webhook(&odd_sender)[0].event, InboundEvent::Ignored(_)));
        assert!(parse_webhook(&json!({"object": "page", "entry": []})).is_empty());
        assert!(parse_webhook(&json!("text")).is_empty());
    }

    #[test]
    fn a_delivery_must_be_signed_with_the_app_secret() {
        let secret = Secret::new("app-secret");
        let body = br#"{"object":"whatsapp_business_account"}"#;
        let signature = sign(body, &secret);
        assert!(verify_signature(body, Some(&signature), &secret).is_ok());
        let changed = br#"{"object":"whatsapp_business_account" }"#;
        assert!(matches!(verify_signature(changed, Some(&signature), &secret), Err(ChannelError::BadSignature(_))));
        let other = sign(body, &Secret::new("another-secret"));
        assert!(matches!(verify_signature(body, Some(&other), &secret), Err(ChannelError::BadSignature(_))));
        assert!(matches!(verify_signature(body, None, &secret), Err(ChannelError::MissingSignature(_))));
        for malformed in ["", "sha256=", "sha256=zz", "sha1=abcd", &signature.replace("sha256=", "")] {
            assert!(verify_signature(body, Some(malformed), &secret).is_err(), "{malformed}");
        }
        // No configured secret never verifies.
        let empty = Secret::new("");
        assert!(verify_signature(body, Some(&sign(body, &empty)), &empty).is_err());
    }

    #[test]
    fn the_subscription_check_echoes_the_challenge_for_the_right_token_only() {
        let token = Secret::new("my-verify-token");
        assert_eq!(
            verify_subscription(Some("subscribe"), Some("my-verify-token"), Some("1158201444"), &token).ok(),
            Some("1158201444".to_owned())
        );
        assert!(verify_subscription(Some("subscribe"), Some("wrong"), Some("1"), &token).is_err());
        assert!(verify_subscription(Some("unsubscribe"), Some("my-verify-token"), Some("1"), &token).is_err());
        assert!(verify_subscription(Some("subscribe"), None, Some("1"), &token).is_err());
        assert!(verify_subscription(Some("subscribe"), Some(""), Some("1"), &Secret::new("")).is_err());
    }

    #[test]
    fn the_service_window_closes_a_day_after_the_last_message() {
        let hour = 60 * 60 * 1000;
        assert!(service_window_open(0, 23 * hour));
        assert!(!service_window_open(0, 24 * hour));
        assert!(!service_window_open(0, 24 * hour - 30_000), "the last minute is kept back");
        assert!(service_window_open(5 * hour, 4 * hour), "a clock that runs behind does not close it");
    }
}
