//! Messaging channels for graphrt agents: Telegram (Bot API), WhatsApp (Cloud
//! API), Slack (Events API and Socket Mode) and Microsoft Teams (Bot Framework).
//!
//! The crate is the part every host shares, and nothing more. It does not start
//! runs, keep conversations or decide who may write: `graphrt serve` and the
//! Desktop app do that, each in its own way, on top of the same three things.
//!
//! 1. **Receive.** [`telegram::parse_update`] and [`whatsapp::parse_webhook`] turn
//!    what the channel delivers into [`InboundEvent`]s. [`telegram::verify_secret_token`],
//!    [`whatsapp::verify_signature`] and [`whatsapp::verify_subscription`] are the
//!    checks a webhook endpoint runs first, on the raw body. Slack and Teams have
//!    the same pair: `slack::verify_signature` with `slack::parse_delivery`, and
//!    `teams::verify_token` with `teams::parse_activity` (features `slack`, `teams`).
//! 2. **Write.** [`text::split`] cuts an answer to the channel's length limit
//!    without breaking a code block, and [`text::to_telegram_html`] and
//!    [`text::to_whatsapp`] turn the Markdown a model writes into what the channel
//!    shows.
//! 3. **Send** (feature `client`). [`Channel`] is one trait over both channels:
//!    send text with optional buttons, send a file, show that the agent is typing,
//!    download what the customer sent. [`telegram::TelegramBot`] and
//!    [`whatsapp::WhatsAppCloud`] implement it and also expose the calls only
//!    their channel has (long polling, webhook registration, templates).
//!
//! Everything a customer sends is untrusted input. This crate only moves it: it
//! never interprets message text, and it caps what it reads ([`Channel::download`]
//! takes a byte limit).
//!
//! Secrets are held in [`Secret`], which never prints its value, and no error of
//! this crate carries a request URL: a Telegram URL contains the bot token.
//!
//! ```
//! # #[cfg(feature = "telegram")] {
//! use graphrt_channel::{InboundEvent, telegram};
//!
//! let update = serde_json::json!({
//!     "update_id": 7,
//!     "message": {"message_id": 3, "date": 1_700_000_000, "text": "Halo",
//!                 "chat": {"id": 42, "type": "private"},
//!                 "from": {"id": 42, "is_bot": false, "first_name": "Sari"}},
//! });
//! let parsed = telegram::parse_update(&update);
//! let InboundEvent::Message(message) = parsed.event else { unreachable!() };
//! assert_eq!((message.chat_id.as_str(), message.text.as_str()), ("42", "Halo"));
//! assert_eq!(message.customer.name.as_deref(), Some("Sari"));
//! # }
//! ```

#[cfg(any(feature = "slack", feature = "teams"))]
mod card;
#[cfg(feature = "client")]
mod channel;
#[cfg(all(feature = "client", any(feature = "telegram", feature = "whatsapp", feature = "slack", feature = "teams")))]
mod http;
mod model;
pub mod sign;
#[cfg(feature = "slack")]
pub mod slack;
#[cfg(feature = "teams")]
pub mod teams;
#[cfg(feature = "telegram")]
pub mod telegram;
pub mod text;
#[cfg(feature = "whatsapp")]
pub mod whatsapp;

#[cfg(feature = "client")]
pub use channel::{BoxFuture, Channel, Download, OutboundFile, RichMessage, RichPart};
pub use model::{
    Attachment, AttachmentKind, Button, ChannelError, ChannelKind, ChatKind, Customer, DeliveryState, DeliveryStatus,
    InboundEvent, InboundMessage, Secret,
};
