//! Telegram, through the official [Bot API](https://core.telegram.org/bots/api).
//!
//! A bot receives updates in one of two ways, never both at once:
//!
//! - **Long polling** ([`TelegramBot::get_updates`]): the bot asks Telegram and
//!   waits. It needs no public address, so it works on a laptop.
//! - **Webhook** ([`TelegramBot::set_webhook`]): Telegram posts each update to an
//!   HTTPS address, with the secret given at registration in the header
//!   [`SECRET_TOKEN_HEADER`]. Check it with [`verify_secret_token`] before
//!   reading the body.
//!
//! Either way an update is one JSON object, which [`parse_update`] reads.
//! Telegram delivers an update again until it is confirmed (a polling call with
//! a higher `offset`, or a `200` to the webhook): the message ids of
//! [`crate::InboundMessage`] are stable, so a host drops what it has seen.

use serde_json::Value as Json;

use crate::model::{Attachment, AttachmentKind, ChatKind, Customer, InboundEvent, InboundMessage, Secret};
use crate::sign::constant_time_eq;

/// The header that carries the webhook secret.
pub const SECRET_TOKEN_HEADER: &str = "x-telegram-bot-api-secret-token";

/// The update kinds a chat agent needs. Asking only for these keeps edits,
/// reactions and member changes from being delivered at all.
pub const ALLOWED_UPDATES: [&str; 2] = ["message", "callback_query"];

/// Whether a webhook delivery carries the secret set at registration.
/// `header` is the value of [`SECRET_TOKEN_HEADER`].
pub fn verify_secret_token(header: Option<&str>, expected: &Secret) -> bool {
    match header {
        Some(given) if !expected.is_empty() => constant_time_eq(given.as_bytes(), expected.expose().as_bytes()),
        _ => false,
    }
}

/// Whether `secret` is one Telegram accepts for a webhook: 1 to 256 characters
/// of `A-Z`, `a-z`, `0-9`, `_` and `-`.
pub fn is_valid_webhook_secret(secret: &str) -> bool {
    (1..=256).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One update, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedUpdate {
    /// Telegram's number of the update. The next polling call asks from this plus one.
    pub update_id: Option<i64>,
    pub event: InboundEvent,
}

/// Read one update (an element of `getUpdates`, or a webhook body).
pub fn parse_update(update: &Json) -> ParsedUpdate {
    let update_id = update.get("update_id").and_then(Json::as_i64);
    let event = if let Some(message) = update.get("message") {
        message_event(message)
    } else if let Some(query) = update.get("callback_query") {
        callback_event(query)
    } else {
        InboundEvent::Ignored("not a message")
    };
    ParsedUpdate { update_id, event }
}

fn text_of<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).filter(|text| !text.is_empty())
}

/// An id as text: Telegram ids are integers, sometimes beyond 32 bits.
fn id_of(value: &Json, key: &str) -> Option<String> {
    match value.get(key)? {
        Json::Number(number) => Some(number.to_string()),
        Json::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

fn customer_of(from: &Json) -> Option<Customer> {
    let id = id_of(from, "id")?;
    let name = match (text_of(from, "first_name"), text_of(from, "last_name")) {
        (Some(first), Some(last)) => Some(format!("{first} {last}")),
        (Some(first), None) => Some(first.to_owned()),
        (None, _) => text_of(from, "username").map(str::to_owned),
    };
    Some(Customer { id, name })
}

fn chat_kind_of(chat: &Json) -> Option<ChatKind> {
    match text_of(chat, "type") {
        Some("private") => Some(ChatKind::Private),
        Some("group" | "supergroup") => Some(ChatKind::Group),
        _ => None,
    }
}

fn message_event(message: &Json) -> InboundEvent {
    let (Some(chat), Some(from)) = (message.get("chat"), message.get("from")) else {
        return InboundEvent::Ignored("a message without a sender");
    };
    if from.get("is_bot").and_then(Json::as_bool) == Some(true) {
        return InboundEvent::Ignored("written by a bot");
    }
    let (Some(chat_id), Some(chat_kind)) = (id_of(chat, "id"), chat_kind_of(chat)) else {
        return InboundEvent::Ignored("not a private chat or a group");
    };
    let (Some(customer), Some(message_id)) = (customer_of(from), id_of(message, "message_id")) else {
        return InboundEvent::Ignored("a message without ids");
    };
    let text = text_of(message, "text").or_else(|| text_of(message, "caption")).unwrap_or_default();
    let attachments = attachments_of(message);
    const UNREADABLE: [&str; 9] =
        ["sticker", "location", "venue", "contact", "poll", "dice", "game", "video_note", "story"];
    let unsupported = UNREADABLE.iter().find(|kind| message.get(**kind).is_some()).map(|kind| (*kind).to_owned());
    if text.is_empty() && attachments.is_empty() && unsupported.is_none() {
        // Members joining, a pinned message, a changed title.
        return InboundEvent::Ignored("a service message");
    }
    InboundEvent::Message(Box::new(InboundMessage {
        // Message ids count per chat.
        id: format!("{chat_id}:{message_id}"),
        chat_id,
        chat_kind,
        customer,
        text: text.to_owned(),
        attachments,
        choice: None,
        reference: None,
        timestamp_ms: message.get("date").and_then(Json::as_u64).map(|seconds| seconds.saturating_mul(1000)),
        unsupported,
    }))
}

fn attachments_of(message: &Json) -> Vec<Attachment> {
    let mut attachments = Vec::new();
    let mut add = |file: &Json, kind: AttachmentKind, default_mime: Option<&str>| {
        if let Some(file_id) = text_of(file, "file_id") {
            attachments.push(Attachment {
                file_id: file_id.to_owned(),
                kind,
                name: text_of(file, "file_name").map(str::to_owned),
                mime_type: text_of(file, "mime_type").or(default_mime).map(str::to_owned),
                size: file.get("file_size").and_then(Json::as_u64),
            });
        }
    };
    // A photo comes in several sizes, smallest first: the last one is the picture.
    if let Some(largest) = message.get("photo").and_then(Json::as_array).and_then(|sizes| sizes.last()) {
        add(largest, AttachmentKind::Image, Some("image/jpeg"));
    }
    if let Some(document) = message.get("document") {
        add(document, AttachmentKind::Document, None);
    }
    if let Some(audio) = message.get("audio") {
        add(audio, AttachmentKind::Audio, None);
    }
    if let Some(voice) = message.get("voice") {
        add(voice, AttachmentKind::Audio, Some("audio/ogg"));
    }
    // An animation is also sent as `document`, which is already taken.
    if let Some(video) = message.get("video") {
        add(video, AttachmentKind::Video, Some("video/mp4"));
    }
    attachments
}

fn callback_event(query: &Json) -> InboundEvent {
    let (Some(query_id), Some(from)) = (text_of(query, "id"), query.get("from")) else {
        return InboundEvent::Ignored("a button tap without ids");
    };
    let Some(customer) = customer_of(from) else {
        return InboundEvent::Ignored("a button tap without a sender");
    };
    let chat = query.get("message").and_then(|message| message.get("chat"));
    let chat_id = chat.and_then(|chat| id_of(chat, "id")).unwrap_or_else(|| customer.id.clone());
    let chat_kind = chat.and_then(chat_kind_of).unwrap_or(ChatKind::Private);
    let Some(data) = text_of(query, "data") else {
        return InboundEvent::Ignored("a button tap without data");
    };
    InboundEvent::Message(Box::new(InboundMessage {
        id: format!("cb:{query_id}"),
        chat_id,
        chat_kind,
        customer,
        text: data.to_owned(),
        attachments: Vec::new(),
        choice: Some(data.to_owned()),
        reference: Some(format!("cb:{query_id}")),
        timestamp_ms: None,
        unsupported: None,
    }))
}

/// The command a message starts with and the text after it: `/start hello`
/// gives `("start", "hello")`. A command addressed to a bot (`/start@my_bot`)
/// loses the address. `None` when the text is not a command.
pub fn command(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (word, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = word.split('@').next().unwrap_or(word);
    let is_name = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    is_name.then_some((name, argument.trim()))
}

#[cfg(feature = "client")]
pub use client::{BotIdentity, TelegramBot};

#[cfg(feature = "client")]
mod client {
    use std::time::Duration;

    use serde_json::{Value as Json, json};

    use super::{ALLOWED_UPDATES, text_of};
    use crate::channel::{BoxFuture, Channel, Download, OutboundFile};
    use crate::http::{self, Answer, CALL_TIMEOUT, MAX_ANSWER_BYTES, Multipart, TRANSFER_TIMEOUT, safe_file_name};
    use crate::model::{Attachment, Button, ChannelError, ChannelKind, InboundMessage, Secret};
    use crate::text;

    /// Where the Bot API lives.
    pub const API_BASE: &str = "https://api.telegram.org";
    /// Source text per message. Telegram takes 4096 units after it removed the
    /// marks: the margin covers the few conversions that add characters.
    const TEXT_UNITS: usize = 4000;
    /// Source text of a caption (Telegram takes 1024 units).
    const CAPTION_UNITS: usize = 1000;
    /// Largest file a bot may send, and largest picture sent as a photo.
    const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
    const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;
    /// Longest `callback_data` of a button, in bytes.
    const MAX_CALLBACK_BYTES: usize = 64;
    /// Longest wait Telegram may ask for that a call still honours by itself.
    const MAX_RETRY_AFTER_SECS: u64 = 30;
    const RETRIES: usize = 2;
    const KIND: ChannelKind = ChannelKind::Telegram;

    /// Who the bot is (`getMe`).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct BotIdentity {
        pub id: String,
        /// Without the `@`.
        pub username: Option<String>,
        pub name: Option<String>,
    }

    /// One Telegram bot.
    #[derive(Clone, Debug)]
    pub struct TelegramBot {
        http: reqwest::Client,
        base: String,
        token: Secret,
    }

    impl TelegramBot {
        /// The bot of `token` (from @BotFather), calling through `http`.
        ///
        /// @throws ChannelError - `Invalid` when `token` has characters no bot token has.
        pub fn new(http: reqwest::Client, token: Secret) -> Result<Self, ChannelError> {
            // The token is a path segment of every request.
            let shaped = token.expose().len() >= 8
                && token.expose().bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'));
            if !shaped {
                return Err(ChannelError::Invalid(
                    "the Telegram bot token is not a token (expected the form 123456:ABC-DEF from @BotFather)"
                        .to_owned(),
                ));
            }
            Ok(Self { http, base: API_BASE.to_owned(), token })
        }

        /// Call another server instead of `api.telegram.org`: a local Bot API
        /// server, or a fake in tests.
        pub fn with_base_url(mut self, base: &str) -> Self {
            self.base = base.trim_end_matches('/').to_owned();
            self
        }

        fn method_url(&self, method: &str) -> String {
            format!("{}/bot{}/{method}", self.base, self.token.expose())
        }

        /// The error of an answer that is not `{"ok": true}`.
        fn refusal(&self, method: &str, answer: &Answer) -> ChannelError {
            let body = answer.json();
            let description = text_of(&body, "description").unwrap_or("no description");
            ChannelError::Api {
                channel: KIND,
                method: method.to_owned(),
                status: answer.status,
                code: body.get("error_code").and_then(Json::as_i64),
                description: self.token.redact(&text::truncate(description, 300)),
                retry_after: body.get("parameters").and_then(|p| p.get("retry_after")).and_then(Json::as_u64),
            }
        }

        /// The `result` of an answer, or its refusal.
        fn result_of(&self, method: &str, answer: &Answer) -> Result<Json, ChannelError> {
            let mut body = answer.json();
            if answer.status == 200 && body.get("ok").and_then(Json::as_bool) == Some(true) {
                return Ok(body.get_mut("result").map(Json::take).unwrap_or(Json::Null));
            }
            Err(self.refusal(method, answer))
        }

        /// Call `method` with JSON `params`. A `429` with a short `retry_after` is waited out.
        async fn call(&self, method: &str, params: &Json, timeout: Duration) -> Result<Json, ChannelError> {
            let body = serde_json::to_vec(params).map_err(|e| ChannelError::Invalid(e.to_string()))?;
            let mut attempt = 0;
            loop {
                let request = self
                    .http
                    .post(self.method_url(method))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone());
                let answer = http::send(KIND, request, timeout, MAX_ANSWER_BYTES).await?;
                match self.result_of(method, &answer) {
                    Err(ChannelError::Api { retry_after: Some(wait), .. })
                        if attempt < RETRIES && wait <= MAX_RETRY_AFTER_SECS =>
                    {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
                    }
                    result => return result,
                }
            }
        }

        fn bad_answer(method: &str, reason: &str) -> ChannelError {
            ChannelError::BadAnswer { channel: KIND, method: method.to_owned(), reason: reason.to_owned() }
        }

        /// Who the bot is. Also the cheapest way to check a token.
        ///
        /// @throws ChannelError - `Api` with status 401 for a wrong token.
        pub async fn get_me(&self) -> Result<BotIdentity, ChannelError> {
            let me = self.call("getMe", &json!({}), CALL_TIMEOUT).await?;
            let id = super::id_of(&me, "id").ok_or_else(|| Self::bad_answer("getMe", "no id"))?;
            Ok(BotIdentity {
                id,
                username: text_of(&me, "username").map(str::to_owned),
                name: text_of(&me, "first_name").map(str::to_owned),
            })
        }

        /// Wait up to `wait_secs` for updates numbered `offset` or higher (long
        /// polling). Asking from an offset confirms every update below it.
        ///
        /// @throws ChannelError - `Api` with status 409 while a webhook is set or another process polls.
        pub async fn get_updates(&self, offset: Option<i64>, wait_secs: u64) -> Result<Vec<Json>, ChannelError> {
            let mut params = json!({ "timeout": wait_secs, "allowed_updates": ALLOWED_UPDATES });
            if let (Some(offset), Some(object)) = (offset, params.as_object_mut()) {
                object.insert("offset".to_owned(), json!(offset));
            }
            let timeout = Duration::from_secs(wait_secs.saturating_add(10));
            match self.call("getUpdates", &params, timeout).await? {
                Json::Array(updates) => Ok(updates),
                _ => Err(Self::bad_answer("getUpdates", "not a list")),
            }
        }

        /// The address Telegram posts updates to, when a webhook is set.
        pub async fn webhook_url(&self) -> Result<Option<String>, ChannelError> {
            let info = self.call("getWebhookInfo", &json!({}), CALL_TIMEOUT).await?;
            Ok(text_of(&info, "url").map(str::to_owned))
        }

        /// Have Telegram post updates to `url` (HTTPS), each with `secret` in
        /// the header [`super::SECRET_TOKEN_HEADER`].
        ///
        /// @throws ChannelError - `Invalid` for a secret Telegram does not accept.
        pub async fn set_webhook(&self, url: &str, secret: &Secret) -> Result<(), ChannelError> {
            if !super::is_valid_webhook_secret(secret.expose()) {
                return Err(ChannelError::Invalid(
                    "a Telegram webhook secret is 1 to 256 characters of A-Z, a-z, 0-9, _ and -".to_owned(),
                ));
            }
            let params = json!({ "url": url, "secret_token": secret.expose(), "allowed_updates": ALLOWED_UPDATES });
            self.call("setWebhook", &params, CALL_TIMEOUT).await.map(|_| ())
        }

        /// Remove the webhook, so [`TelegramBot::get_updates`] works. Updates
        /// that wait are kept.
        pub async fn delete_webhook(&self) -> Result<(), ChannelError> {
            self.call("deleteWebhook", &json!({}), CALL_TIMEOUT).await.map(|_| ())
        }

        /// Send one message of at most 4096 units. `html` says `text` is
        /// Telegram HTML. Buttons go under it, one per row.
        ///
        /// @returns The message id.
        pub async fn send_message(
            &self,
            chat_id: &str,
            text: &str,
            html: bool,
            buttons: &[Button],
        ) -> Result<String, ChannelError> {
            let mut params =
                json!({ "chat_id": chat_id, "text": text, "link_preview_options": { "is_disabled": true } });
            if let Some(object) = params.as_object_mut() {
                if html {
                    object.insert("parse_mode".to_owned(), json!("HTML"));
                }
                if let Some(keyboard) = keyboard_of(buttons) {
                    object.insert("reply_markup".to_owned(), keyboard);
                }
            }
            let sent = self.call("sendMessage", &params, CALL_TIMEOUT).await?;
            super::id_of(&sent, "message_id").ok_or_else(|| Self::bad_answer("sendMessage", "no message id"))
        }

        /// Show `action` (`typing`, `upload_document`...) in the chat for five seconds.
        pub async fn send_chat_action(&self, chat_id: &str, action: &str) -> Result<(), ChannelError> {
            self.call("sendChatAction", &json!({ "chat_id": chat_id, "action": action }), CALL_TIMEOUT)
                .await
                .map(|_| ())
        }

        /// Tell Telegram a button tap arrived, which ends the button's spinner.
        pub async fn answer_callback_query(&self, query_id: &str) -> Result<(), ChannelError> {
            self.call("answerCallbackQuery", &json!({ "callback_query_id": query_id }), CALL_TIMEOUT).await.map(|_| ())
        }

        async fn upload(&self, chat_id: &str, file: &OutboundFile) -> Result<String, ChannelError> {
            if file.bytes.len() > MAX_UPLOAD_BYTES {
                return Err(ChannelError::TooLarge { limit: u64::try_from(MAX_UPLOAD_BYTES).unwrap_or(u64::MAX) });
            }
            let as_photo = file.is_image() && file.bytes.len() <= MAX_PHOTO_BYTES;
            let (method, field) = if as_photo { ("sendPhoto", "photo") } else { ("sendDocument", "document") };
            let mut form = Multipart::new().text("chat_id", chat_id);
            if let Some(caption) = file.caption.as_deref().filter(|caption| !caption.trim().is_empty()) {
                let caption = text::to_telegram_html(&text::truncate(caption.trim(), CAPTION_UNITS));
                form = form.text("caption", &caption).text("parse_mode", "HTML");
            }
            let (content_type, body) =
                form.file(field, &safe_file_name(&file.name), &file.mime_type, &file.bytes).finish();
            let request =
                self.http.post(self.method_url(method)).header(reqwest::header::CONTENT_TYPE, content_type).body(body);
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, MAX_ANSWER_BYTES).await?;
            let sent = self.result_of(method, &answer)?;
            super::id_of(&sent, "message_id").ok_or_else(|| Self::bad_answer(method, "no message id"))
        }

        async fn fetch(&self, attachment: &Attachment, max_bytes: u64) -> Result<Download, ChannelError> {
            if attachment.size.is_some_and(|size| size > max_bytes) {
                return Err(ChannelError::TooLarge { limit: max_bytes });
            }
            let file = self.call("getFile", &json!({ "file_id": attachment.file_id }), CALL_TIMEOUT).await?;
            if file.get("file_size").and_then(Json::as_u64).is_some_and(|size| size > max_bytes) {
                return Err(ChannelError::TooLarge { limit: max_bytes });
            }
            let path = text_of(&file, "file_path").ok_or_else(|| Self::bad_answer("getFile", "no file path"))?;
            // The path comes from Telegram: it still must not climb out of the bot's files.
            if path.split('/').any(|segment| segment.is_empty() || segment == "..") {
                return Err(Self::bad_answer("getFile", "a file path that is not one"));
            }
            let url = format!("{}/file/bot{}/{path}", self.base, self.token.expose());
            let answer = http::send(KIND, self.http.get(url), TRANSFER_TIMEOUT, max_bytes).await?;
            if answer.status != 200 {
                return Err(ChannelError::Api {
                    channel: KIND,
                    method: "file download".to_owned(),
                    status: answer.status,
                    code: None,
                    description: "the file could not be downloaded".to_owned(),
                    retry_after: None,
                });
            }
            let name = attachment.name.clone().or_else(|| path.rsplit('/').next().map(str::to_owned));
            Ok(Download { bytes: answer.body, mime_type: attachment.mime_type.clone(), name })
        }

        async fn send_parts(
            &self,
            chat_id: &str,
            markdown: &str,
            buttons: &[Button],
        ) -> Result<Vec<String>, ChannelError> {
            let parts = text::split(markdown, TEXT_UNITS);
            let mut sent = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                let buttons = if index + 1 == parts.len() { buttons } else { &[] };
                let html = text::to_telegram_html(part);
                let id = match self.send_message(chat_id, &html, true, buttons).await {
                    // Telegram could not read the formatting: the customer still gets the words.
                    Err(ChannelError::Api { status: 400, .. }) => {
                        self.send_message(chat_id, part, false, buttons).await?
                    }
                    other => other?,
                };
                sent.push(id);
            }
            Ok(sent)
        }
    }

    /// An inline keyboard with one button per row, or `None` when there are no
    /// buttons or one of them cannot be a button (its id is longer than Telegram allows).
    fn keyboard_of(buttons: &[Button]) -> Option<Json> {
        if buttons.is_empty() || buttons.iter().any(|b| b.id.is_empty() || b.id.len() > MAX_CALLBACK_BYTES) {
            return None;
        }
        let rows: Vec<Json> = buttons
            .iter()
            .map(|button| json!([{ "text": text::truncate(&button.title, 60), "callback_data": button.id }]))
            .collect();
        Some(json!({ "inline_keyboard": rows }))
    }

    impl Channel for TelegramBot {
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

        fn typing<'a>(&'a self, message: &'a InboundMessage) -> BoxFuture<'a, Result<(), ChannelError>> {
            Box::pin(async move {
                if let Some(query_id) = message.reference.as_deref().and_then(|r| r.strip_prefix("cb:")) {
                    // Best effort: an old tap can no longer be answered.
                    let _answered = self.answer_callback_query(query_id).await;
                }
                self.send_chat_action(&message.chat_id, "typing").await
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

        #[test]
        fn a_token_must_look_like_one() {
            let http = reqwest::Client::new();
            assert!(TelegramBot::new(http.clone(), Secret::new("123456:AAE-xample_token")).is_ok());
            for bad in ["", "short", "123456:abc/../../x", "123456:abc def"] {
                let refused = TelegramBot::new(http.clone(), Secret::new(bad));
                assert!(matches!(refused, Err(ChannelError::Invalid(_))), "{bad}");
            }
        }

        #[test]
        fn a_refusal_never_shows_the_token() {
            let bot = TelegramBot::new(reqwest::Client::new(), Secret::new("123456:AAE-xample_token"))
                .unwrap_or_else(|e| unreachable!("{e}"));
            let body = br#"{"ok":false,"error_code":404,"description":"Not Found: /bot123456:AAE-xample_token/x"}"#;
            let answer = Answer { status: 404, content_type: None, body: body.to_vec() };
            let text = bot.refusal("getMe", &answer).to_string();
            assert!(!text.contains("AAE-xample_token"), "{text}");
            assert!(text.contains("`getMe` was refused with 404"), "{text}");
            assert!(!format!("{bot:?}").contains("AAE-xample_token"));
        }

        #[test]
        fn buttons_become_a_keyboard_unless_one_cannot() {
            let button = |id: &str| Button { id: id.to_owned(), title: "Approve".to_owned() };
            let keyboard = keyboard_of(&[button("1"), button("2")]).unwrap_or_default();
            assert_eq!(keyboard["inline_keyboard"][1][0]["callback_data"], "2");
            assert_eq!(keyboard_of(&[]), None);
            assert_eq!(keyboard_of(&[button(&"x".repeat(65))]), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(event: InboundEvent) -> InboundMessage {
        match event {
            InboundEvent::Message(message) => *message,
            other => unreachable!("expected a message, got {other:?}"),
        }
    }

    fn private(extra: Json) -> Json {
        let mut update = json!({
            "update_id": 100,
            "message": {
                "message_id": 5, "date": 1_700_000_000,
                "chat": {"id": 42, "type": "private"},
                "from": {"id": 42, "is_bot": false, "first_name": "Sari", "last_name": "Dewi", "username": "sari"},
            },
        });
        if let (Some(target), Some(source)) = (update["message"].as_object_mut(), extra.as_object()) {
            target.extend(source.clone());
        }
        update
    }

    #[test]
    fn a_text_message_names_its_chat_customer_and_id() {
        let parsed = parse_update(&private(json!({"text": "Halo"})));
        assert_eq!(parsed.update_id, Some(100));
        let message = message(parsed.event);
        assert_eq!(message.id, "42:5");
        assert_eq!((message.chat_id.as_str(), message.chat_kind), ("42", ChatKind::Private));
        assert_eq!(message.customer, Customer { id: "42".into(), name: Some("Sari Dewi".into()) });
        assert_eq!(message.text, "Halo");
        assert_eq!(message.timestamp_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn files_come_with_their_caption_and_the_largest_photo_size() {
        let photo = json!({"caption": "the receipt", "photo": [
            {"file_id": "small", "file_size": 100}, {"file_id": "large", "file_size": 90_000}]});
        let message_with_photo = message(parse_update(&private(photo)).event);
        assert_eq!(message_with_photo.text, "the receipt");
        assert_eq!(message_with_photo.attachments.len(), 1);
        let picture = &message_with_photo.attachments[0];
        assert_eq!(
            (picture.file_id.as_str(), picture.kind, picture.size),
            ("large", AttachmentKind::Image, Some(90_000))
        );
        assert_eq!(picture.mime_type.as_deref(), Some("image/jpeg"));

        let document = json!({"document": {"file_id": "d1", "file_name": "invoice.pdf",
            "mime_type": "application/pdf", "file_size": 2048}});
        let with_document = message(parse_update(&private(document)).event);
        assert_eq!(with_document.text, "");
        assert_eq!(with_document.attachments[0].name.as_deref(), Some("invoice.pdf"));
        let voice = message(parse_update(&private(json!({"voice": {"file_id": "v1"}}))).event);
        assert_eq!(voice.attachments[0].kind, AttachmentKind::Audio);
    }

    #[test]
    fn a_group_message_answers_in_the_group_and_names_who_wrote() {
        let mut update = private(json!({"text": "/ask@my_bot hi"}));
        update["message"]["chat"] = json!({"id": -100_123, "type": "supergroup", "title": "Team"});
        let message = message(parse_update(&update).event);
        assert_eq!((message.chat_id.as_str(), message.chat_kind), ("-100123", ChatKind::Group));
        assert_eq!(message.customer.id, "42");
        assert_eq!(message.id, "-100123:5");
    }

    #[test]
    fn a_button_tap_is_a_message_with_a_choice() {
        let update = json!({"update_id": 101, "callback_query": {
            "id": "777", "data": "2",
            "from": {"id": 42, "is_bot": false, "username": "sari"},
            "message": {"message_id": 9, "chat": {"id": 42, "type": "private"}},
        }});
        let message = message(parse_update(&update).event);
        assert_eq!(message.id, "cb:777");
        assert_eq!(message.choice.as_deref(), Some("2"));
        assert_eq!(message.reference.as_deref(), Some("cb:777"));
        assert_eq!(message.customer.name.as_deref(), Some("sari"));
    }

    #[test]
    fn what_needs_no_answer_is_ignored_with_a_reason() {
        let mut from_bot = private(json!({"text": "beep"}));
        from_bot["message"]["from"]["is_bot"] = json!(true);
        let mut channel_post = private(json!({"text": "news"}));
        channel_post["message"]["chat"]["type"] = json!("channel");
        let joined = private(json!({"new_chat_members": [{"id": 1}]}));
        let edited = json!({"update_id": 1, "edited_message": {"message_id": 1}});
        for update in [from_bot, channel_post, joined, edited, json!({}), json!("text")] {
            assert!(matches!(parse_update(&update).event, InboundEvent::Ignored(_)), "{update}");
        }
    }

    #[test]
    fn a_message_an_agent_cannot_read_says_what_it_is() {
        let location = message(parse_update(&private(json!({"location": {"latitude": 1.0, "longitude": 2.0}}))).event);
        assert_eq!(location.unsupported.as_deref(), Some("location"));
        assert!(location.text.is_empty() && location.attachments.is_empty());
    }

    #[test]
    fn the_webhook_secret_must_match_exactly() {
        let secret = Secret::new("s3cret-token_1");
        assert!(verify_secret_token(Some("s3cret-token_1"), &secret));
        assert!(!verify_secret_token(Some("s3cret-token_2"), &secret));
        assert!(!verify_secret_token(Some(""), &secret));
        assert!(!verify_secret_token(None, &secret));
        // No configured secret never verifies, whatever is sent.
        assert!(!verify_secret_token(Some(""), &Secret::new("")));
        assert!(is_valid_webhook_secret("abc_DEF-123"));
        assert!(!is_valid_webhook_secret("has space"));
        assert!(!is_valid_webhook_secret(""));
    }

    #[test]
    fn commands_are_split_from_their_argument() {
        assert_eq!(command("/start"), Some(("start", "")));
        assert_eq!(command("  /start@my_bot  ref_42 "), Some(("start", "ref_42")));
        assert_eq!(command("/new\nplease"), Some(("new", "please")));
        assert_eq!(command("not /a command"), None);
        assert_eq!(command("/"), None);
        assert_eq!(command("/12.5 is the price"), None);
    }
}
