//! [`Channel`]: what a host does with any channel once a customer has written.

use std::future::Future;
use std::pin::Pin;

use crate::model::{Attachment, Button, ChannelError, ChannelKind, InboundMessage};

/// A boxed future, so [`Channel`] can be used as `dyn Channel`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A file to send to a customer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundFile {
    /// The name the customer sees.
    pub name: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    /// Text shown with the file, in Markdown. Cut to the channel's caption limit.
    pub caption: Option<String>,
}

impl OutboundFile {
    /// Whether the channel can show the file as a picture instead of a document.
    pub fn is_image(&self) -> bool {
        matches!(self.mime_type.as_str(), "image/jpeg" | "image/png")
    }
}

/// A file a customer sent, fetched from the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub bytes: Vec<u8>,
    /// The type the channel reports for the bytes. Untrusted.
    pub mime_type: Option<String>,
    /// A file name, when the channel knows one. Untrusted.
    pub name: Option<String>,
}

/// One part of a [`RichMessage`].
#[derive(Clone, Debug, PartialEq)]
pub enum RichPart {
    /// Markdown, as [`Channel::send_text`] takes it.
    Markdown(String),
    /// An [Adaptive Card](https://adaptivecards.io) that a host has validated
    /// (graphrt-ui does), for a channel that can draw one: Teams shows it, Slack
    /// draws what Block Kit can show of it. `fallback` is the card as Markdown,
    /// with its choices numbered, for a channel or a card that cannot.
    Card {
        card: serde_json::Value,
        fallback: String,
        /// The card's answers, in the order of its submit actions (or of the
        /// options of its one choice input). A tap comes back as an
        /// [`InboundMessage`] whose `choice` is the button's `id`.
        choices: Vec<Button>,
    },
}

impl RichPart {
    /// The part as Markdown.
    pub fn markdown(&self) -> &str {
        match self {
            RichPart::Markdown(text) => text,
            RichPart::Card { fallback, .. } => fallback,
        }
    }
}

/// A message with parts a channel may draw natively (see [`Channel::send_rich`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichMessage {
    pub parts: Vec<RichPart>,
    /// Buttons under the whole message, as for [`Channel::send_text`].
    pub buttons: Vec<Button>,
}

impl RichMessage {
    /// Every part as Markdown, one paragraph each.
    pub fn markdown(&self) -> String {
        let parts: Vec<&str> =
            self.parts.iter().map(RichPart::markdown).map(str::trim).filter(|part| !part.is_empty()).collect();
        parts.join("\n\n")
    }

    /// Every button of the message: those of its cards, then its own.
    pub fn all_buttons(&self) -> Vec<Button> {
        let mut buttons: Vec<Button> = self
            .parts
            .iter()
            .flat_map(|part| match part {
                RichPart::Card { choices, .. } => choices.clone(),
                RichPart::Markdown(_) => Vec::new(),
            })
            .collect();
        buttons.extend(self.buttons.iter().cloned());
        buttons
    }

    /// Whether the message says nothing.
    pub fn is_empty(&self) -> bool {
        self.markdown().trim().is_empty()
    }
}

/// One connected channel account (a Telegram bot, a WhatsApp phone number).
///
/// The methods are what both channels can do. What only one of them has
/// (Telegram's long polling, WhatsApp's templates) is on the concrete types.
pub trait Channel: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> ChannelKind;

    /// Send `markdown` to `chat_id`: converted to the channel's formatting and
    /// split into as many messages as its length limit needs. `buttons` go under
    /// the last message where the channel can show them (see each channel's
    /// limits). When it cannot, they are left out: write the choices into the
    /// text as well, numbered, so the customer can always answer by typing.
    ///
    /// @returns The channel's ids of the messages sent.
    /// @throws ChannelError - When a message was refused. Messages sent before it stay sent.
    fn send_text<'a>(
        &'a self,
        chat_id: &'a str,
        markdown: &'a str,
        buttons: &'a [Button],
    ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>>;

    /// Send a file to `chat_id`: as a picture when it is one the channel shows, else as a document.
    ///
    /// @returns The channel's id of the message.
    /// @throws ChannelError - When the upload or the message was refused.
    fn send_file<'a>(&'a self, chat_id: &'a str, file: &'a OutboundFile)
    -> BoxFuture<'a, Result<String, ChannelError>>;

    /// Show the customer of `message` that an answer is being written. The sign
    /// lasts a few seconds (Telegram 5, WhatsApp 25): call again while the work
    /// goes on. On WhatsApp this also marks the message as read.
    ///
    /// @throws ChannelError - When the channel refused. Nothing depends on it: log and go on.
    fn typing<'a>(&'a self, message: &'a InboundMessage) -> BoxFuture<'a, Result<(), ChannelError>>;

    /// Whether [`Channel::send_rich`] draws cards natively. When it does not, a
    /// host can send the same message as text and get the same result.
    fn draws_cards(&self) -> bool {
        false
    }

    /// Send a message whose cards the channel draws natively where it can.
    /// The default sends every part as Markdown, with all buttons under the last
    /// message, which is what a channel without cards shows.
    ///
    /// @returns The channel's ids of the messages sent.
    /// @throws ChannelError - When a message was refused.
    fn send_rich<'a>(
        &'a self,
        chat_id: &'a str,
        message: &'a RichMessage,
    ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>> {
        Box::pin(async move {
            let (markdown, buttons) = (message.markdown(), message.all_buttons());
            self.send_text(chat_id, &markdown, &buttons).await
        })
    }

    /// The chat to write to `user_id` in, opening one when the channel needs that
    /// first (Teams). Telegram, WhatsApp and Slack write to a user by their id,
    /// so the default is the id itself.
    ///
    /// @throws ChannelError - When the channel refused to open a chat.
    fn direct_chat<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<String, ChannelError>> {
        Box::pin(async move { Ok(user_id.to_owned()) })
    }

    /// Fetch a file the customer sent, reading at most `max_bytes`.
    ///
    /// @throws ChannelError - `TooLarge` beyond `max_bytes`, or the channel's refusal.
    fn download<'a>(
        &'a self,
        attachment: &'a Attachment,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Download, ChannelError>>;
}
