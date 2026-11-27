# graphrt-channel

Messaging channels for [Graphrt](https://github.com/Graphrt/graphrt) agents: Telegram
(official Bot API) and WhatsApp (official Cloud API).

It is the part every host shares. `graphrt serve --channel` and the Desktop app both
build on it, each with its own conversations and runs.

| Feature | What it adds |
| --- | --- |
| `telegram` | `telegram::parse_update`, the webhook secret check, commands |
| `whatsapp` | `whatsapp::parse_webhook`, `verify_signature`, `verify_subscription`, the 24 hour window |
| `client` | The `Channel` trait, `TelegramBot` and `WhatsAppCloud`: send, upload, download, long polling |

Without `client` the crate links no HTTP client: it parses, verifies and formats.
With it, the caller brings the `reqwest::Client`, so TLS and proxies stay its choice.

```rust
use graphrt_channel::{Channel, InboundEvent, Secret, telegram};

async fn echo(http: reqwest::Client, token: &str) -> Result<(), graphrt_channel::ChannelError> {
    let bot = telegram::TelegramBot::new(http, Secret::new(token))?;
    let mut offset = None;
    loop {
        for update in bot.get_updates(offset, 25).await? {
            let parsed = telegram::parse_update(&update);
            offset = parsed.update_id.map(|id| id + 1).or(offset);
            if let InboundEvent::Message(message) = parsed.event {
                bot.typing(&message).await.ok();
                bot.send_text(&message.chat_id, &format!("You said: **{}**", message.text), &[]).await?;
            }
        }
    }
}
```

`text::split`, `text::to_telegram_html` and `text::to_whatsapp` are available without
any feature: they cut an answer to the channel's length limit and turn Markdown into
what the channel shows.

See [docs/channels.md](https://github.com/Graphrt/graphrt/blob/main/docs/channels.md).
