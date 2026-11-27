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

    /// Fetch a file the customer sent, reading at most `max_bytes`.
    ///
    /// @throws ChannelError - `TooLarge` beyond `max_bytes`, or the channel's refusal.
    fn download<'a>(
        &'a self,
        attachment: &'a Attachment,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Download, ChannelError>>;
}
