//! What every channel has in common: who wrote, what they sent, what went wrong.

/// A messaging channel this crate speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChannelKind {
    /// Telegram, through the Bot API.
    Telegram,
    /// WhatsApp, through the Cloud API of the WhatsApp Business Platform.
    WhatsApp,
}

impl ChannelKind {
    /// The name used in configuration, conversation ids and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::WhatsApp => "whatsapp",
        }
    }

    /// The kind named `name` (`telegram`, `whatsapp`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "telegram" => Some(ChannelKind::Telegram),
            "whatsapp" => Some(ChannelKind::WhatsApp),
            _ => None,
        }
    }

    /// Longest text of one message, in UTF-16 code units (what both channels count).
    pub fn max_text_units(self) -> usize {
        4096
    }

    /// Longest caption of a file, in UTF-16 code units.
    pub fn max_caption_units(self) -> usize {
        1024
    }
}

impl std::fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A credential (bot token, access token, app secret). `Debug` and `Display`
/// never show it, so it cannot reach a log through a formatted struct.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `text` with every occurrence of the secret replaced, for text that came
    /// from outside (an API error description) and is about to be shown.
    pub fn redact(&self, text: &str) -> String {
        if self.0.len() < 8 { text.to_owned() } else { text.replace(&self.0, "<redacted>") }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Who wrote a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Customer {
    /// Their id on the channel: a Telegram user id, a WhatsApp phone number
    /// (digits, with the country code).
    pub id: String,
    /// The name the channel shows for them, when it gave one. The customer chose
    /// it: treat it as untrusted text.
    pub name: Option<String>,
}

/// Where a message was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatKind {
    /// A chat between one customer and the bot.
    Private,
    /// A group with several people.
    Group,
}

/// What kind of file a customer sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttachmentKind {
    Image,
    Document,
    Audio,
    Video,
    Sticker,
}

/// A file a customer sent. The bytes stay on the channel until
/// [`crate::Channel::download`] fetches them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// The channel's id of the file (Telegram `file_id`, WhatsApp media id).
    pub file_id: String,
    pub kind: AttachmentKind,
    /// The file name the customer's app sent, when it sent one. Untrusted.
    pub name: Option<String>,
    /// The type the channel reports. Untrusted: check the bytes before relying on it.
    pub mime_type: Option<String>,
    /// The size the channel reports, in bytes.
    pub size: Option<u64>,
}

/// One message of a customer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    /// Unique for the channel account. Channels deliver a message again when
    /// they are not sure it arrived: drop a message whose id was seen.
    pub id: String,
    /// The chat to answer in: the customer's own chat, or the group.
    pub chat_id: String,
    pub chat_kind: ChatKind,
    pub customer: Customer,
    /// The text, or the caption of a file. Empty when there is none.
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// The id of the button the customer tapped, when this message is a tap on a
    /// [`Button`] sent earlier. `text` then holds the button's title.
    pub choice: Option<String>,
    /// The channel's own handle of this message, for [`crate::Channel::typing`]
    /// (WhatsApp marks it as read; Telegram answers a button tap).
    pub reference: Option<String>,
    /// When the customer sent it, as the channel reports.
    pub timestamp_ms: Option<u64>,
    /// Set when the message is of a type that carries nothing an agent can read
    /// (a location, a contact card, a poll): the type's name.
    pub unsupported: Option<String>,
}

/// How far a message the bot sent got (WhatsApp reports this; Telegram does not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryState {
    Sent,
    Delivered,
    Read,
    Failed,
}

/// A delivery report for a message the bot sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryStatus {
    /// The id the send call returned.
    pub message_id: String,
    /// Who it was sent to.
    pub recipient: String,
    pub state: DeliveryState,
    /// The channel's error code and text, for [`DeliveryState::Failed`].
    pub error: Option<String>,
}

/// One thing a channel delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InboundEvent {
    /// A customer wrote.
    Message(Box<InboundMessage>),
    /// A report about a message the bot sent.
    Status(DeliveryStatus),
    /// Something that needs no answer (an edit, a reaction, another bot, a
    /// channel post), with the reason.
    Ignored(&'static str),
}

/// A button under a message. The customer's tap comes back as an
/// [`InboundMessage`] whose `choice` is `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub id: String,
    pub title: String,
}

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelError {
    /// The request did not reach the channel, or its answer did not arrive.
    #[error("{channel}: cannot reach the API ({reason})")]
    Transport { channel: ChannelKind, reason: String },
    /// The channel answered with an error.
    #[error("{channel}: `{method}` was refused with {status}: {description}")]
    Api {
        channel: ChannelKind,
        /// The API method or endpoint, never a URL.
        method: String,
        status: u16,
        /// The channel's own error code, when it sent one.
        code: Option<i64>,
        description: String,
        /// Seconds the channel asks to wait before the next call.
        retry_after: Option<u64>,
    },
    /// The channel only takes template messages for this customer now: their
    /// last message is more than 24 hours old (WhatsApp).
    #[error("whatsapp: the 24 hour customer service window is closed, only a template message can be sent")]
    OutsideServiceWindow,
    /// The channel's answer is not what its documentation says.
    #[error("{channel}: unexpected answer to `{method}`: {reason}")]
    BadAnswer { channel: ChannelKind, method: String, reason: String },
    /// A webhook delivery carries no signature or secret.
    #[error("the delivery carries no {0}")]
    MissingSignature(&'static str),
    /// A webhook delivery's signature or secret does not match.
    #[error("the delivery's {0} does not match")]
    BadSignature(&'static str),
    /// A file is larger than the caller allows.
    #[error("the file is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    /// The message cannot be sent as asked.
    #[error("{0}")]
    Invalid(String),
}

impl ChannelError {
    /// Whether sending the same request again later can work: the channel was
    /// unreachable, busy, or asked to slow down.
    pub fn is_temporary(&self) -> bool {
        match self {
            ChannelError::Transport { .. } => true,
            ChannelError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_and_is_removed_from_foreign_text() {
        let token = Secret::new("123456:AAExampleToken");
        assert_eq!(format!("{token:?}"), "Secret(<redacted>)");
        assert_eq!(token.redact("bad url /bot123456:AAExampleToken/getMe"), "bad url /bot<redacted>/getMe");
        // A secret too short to be one is not searched for: it would cut ordinary words.
        assert_eq!(Secret::new("ab").redact("abc"), "abc");
    }

    #[test]
    fn kinds_have_stable_names() {
        for kind in [ChannelKind::Telegram, ChannelKind::WhatsApp] {
            assert_eq!(ChannelKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ChannelKind::parse("sms"), None);
    }

    #[test]
    fn only_busy_or_unreachable_is_temporary() {
        let api = |status| ChannelError::Api {
            channel: ChannelKind::Telegram,
            method: "sendMessage".into(),
            status,
            code: None,
            description: String::new(),
            retry_after: None,
        };
        assert!(api(429).is_temporary());
        assert!(api(502).is_temporary());
        assert!(!api(400).is_temporary());
        assert!(!ChannelError::OutsideServiceWindow.is_temporary());
    }
}
