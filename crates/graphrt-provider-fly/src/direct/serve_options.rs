//! What a direct deployment turns on in `graphrt serve` besides the API: the
//! chat page, conversations kept by the server, monitoring from a phone and
//! messaging channels (`docs/web.md`, `docs/monitoring.md`, `docs/channels.md`).
//! Each is off unless asked for.

use serde::Serialize;

use super::DirectError;

/// Where the store volume is mounted in the Machine (`graphrt serve --store`).
pub const STORE_PATH: &str = "/data";
/// Name of the Fly volume that holds a deployment's store.
pub const STORE_VOLUME_NAME: &str = "graphrt_store";
/// Days a finished run stays in the store (`graphrt serve --store-ttl-days`),
/// so the volume does not fill up with run records.
pub const STORE_RUN_TTL_DAYS: u32 = 30;
/// Size of a new store volume. Devices, push subscriptions and conversations
/// are small JSON files: the smallest volume Fly offers is plenty.
pub const STORE_VOLUME_GB: u32 = 1;

/// Who may use the chat page (`graphrt serve --chat-access`). `users` needs an
/// identity provider (`--verify-jwt`), which a direct deploy does not set up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatAccess {
    /// Holders of the deployment's API key.
    #[default]
    Key,
    /// Anyone who opens the page, within the guest limits.
    Guests,
}

impl ChatAccess {
    /// The value of `--chat-access`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Guests => "guests",
        }
    }
}

/// The chat page of a deployment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatOptions {
    /// Who may chat.
    pub access: ChatAccess,
    /// Title of the page and the widget (default: the workflow name).
    pub title: Option<String>,
    /// Runs per minute one guest may start (default of `graphrt serve`: 10).
    pub guest_rate_limit: Option<u32>,
    /// Runs per minute all guests together may start (default of `graphrt serve`: 120).
    pub guest_budget: Option<u32>,
}

/// A messaging channel a deployment answers on (`graphrt serve --channel KIND=WORKFLOW`).
/// Its credentials are Fly secrets of the names `graphrt serve` reads, never flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectChannel {
    /// Telegram, by long polling: needs a Machine that keeps running.
    Telegram,
    /// WhatsApp Cloud API: Meta posts to `<public url>/v1/channels/whatsapp/webhook`.
    #[serde(rename = "whatsapp")]
    WhatsApp,
}

impl DirectChannel {
    /// The `KIND` of `--channel KIND=WORKFLOW`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::WhatsApp => "whatsapp",
        }
    }

    /// The secrets `graphrt serve` reads for this channel (docs/channels.md).
    pub fn secret_names(self) -> &'static [&'static str] {
        match self {
            Self::Telegram => &["TELEGRAM_BOT_TOKEN"],
            Self::WhatsApp => {
                &["WHATSAPP_ACCESS_TOKEN", "WHATSAPP_PHONE_NUMBER_ID", "WHATSAPP_APP_SECRET", "WHATSAPP_VERIFY_TOKEN"]
            }
        }
    }
}

/// The `graphrt serve` features of one deploy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServeOptions {
    /// `--chat <workflow>`: the chat page and widget for the deployed agent.
    pub chat: Option<ChatOptions>,
    /// `--cors-origin`: sites whose pages may call the agent (and embed the widget).
    pub cors_origins: Vec<String>,
    /// `--conversations`: the server keeps the chat history.
    pub conversations: bool,
    /// `--monitor`: the monitoring page (turns on devices and approvals).
    pub monitor: bool,
    /// `--devices`: device tokens for phones.
    pub devices: bool,
    /// `--approvals`: the approvals inbox.
    pub approvals: bool,
    /// `--expire-approvals`: requests answer themselves when their time runs out.
    pub expire_approvals: bool,
    /// `--push <contact>`: Web Push notifications.
    pub push: Option<String>,
    /// `--channel <kind>=<workflow>`: messaging channels (each kind once).
    pub channels: Vec<DirectChannel>,
}

impl ServeOptions {
    /// Whether the deployment keeps state that must outlive a Machine stop or a
    /// redeploy: paired devices, push subscriptions with the server's push key,
    /// conversations, or channel conversations (with the message ids that keep a
    /// redelivered message from running twice). Such a deployment gets a volume as its `--store`.
    pub fn needs_store(&self) -> bool {
        self.monitor || self.devices || self.push.is_some() || self.conversations || !self.channels.is_empty()
    }

    /// Whether a device is ever paired (then the pairing link needs the public address).
    fn pairs_devices(&self) -> bool {
        self.monitor || self.devices
    }

    /// Checks the options before any call to Fly.
    ///
    /// @throws DirectError(InvalidInput) - values `graphrt serve` would refuse at startup
    pub fn validate(&self) -> Result<(), DirectError> {
        if let Some(chat) = &self.chat
            && let Some(title) = &chat.title
            && (title.trim().is_empty() || title.len() > 120 || title.chars().any(char::is_control))
        {
            return Err(DirectError::invalid("--chat-title must be 1-120 printable characters"));
        }
        for origin in &self.cors_origins {
            if !is_origin(origin) {
                return Err(DirectError::invalid(format!(
                    "--cors-origin {origin:?} is not an origin such as https://www.example.com (scheme and host, no path)"
                )));
            }
        }
        if let Some(contact) = &self.push
            && !is_push_contact(contact)
        {
            return Err(DirectError::invalid(format!(
                "--push {contact:?} must be an address push services may write to: mailto:you@example.com or an https: URL"
            )));
        }
        let mut kinds = self.channels.clone();
        kinds.sort();
        if kinds.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(DirectError::invalid("--channel names each kind once"));
        }
        Ok(())
    }

    /// The flags added to `graphrt serve <dir> --addr ...` for these options.
    ///
    /// @param workflow - the name the agent is served under (&str)
    /// @param public_url - the deployment's public address, for pairing links (&str)
    /// @param with_store - a volume is mounted at [`STORE_PATH`] (bool)
    pub fn serve_flags(&self, workflow: &str, public_url: &str, with_store: bool) -> Vec<String> {
        let mut flags: Vec<String> = Vec::new();
        let mut push = |flag: &str, value: Option<String>| {
            flags.push(flag.to_owned());
            flags.extend(value);
        };
        if with_store {
            push("--store", Some(STORE_PATH.to_owned()));
            push("--store-ttl-days", Some(STORE_RUN_TTL_DAYS.to_string()));
        }
        if let Some(chat) = &self.chat {
            push("--chat", Some(workflow.to_owned()));
            push("--chat-access", Some(chat.access.as_str().to_owned()));
            if let Some(title) = &chat.title {
                push("--chat-title", Some(title.clone()));
            }
            if let Some(limit) = chat.guest_rate_limit {
                push("--chat-guest-rate-limit", Some(limit.to_string()));
            }
            if let Some(budget) = chat.guest_budget {
                push("--chat-guest-budget", Some(budget.to_string()));
            }
        }
        for origin in &self.cors_origins {
            push("--cors-origin", Some(origin.clone()));
        }
        if self.conversations {
            push("--conversations", None);
        }
        if self.monitor {
            push("--monitor", None);
        }
        if self.devices && !self.monitor {
            push("--devices", None);
        }
        if self.approvals && !self.monitor && !self.expire_approvals {
            push("--approvals", None);
        }
        if self.expire_approvals {
            push("--expire-approvals", None);
        }
        if let Some(contact) = &self.push {
            push("--push", Some(contact.clone()));
        }
        for channel in &self.channels {
            push("--channel", Some(format!("{}={workflow}", channel.as_str())));
        }
        if self.pairs_devices() {
            push("--public-url", Some(public_url.to_owned()));
        }
        flags
    }

    /// The options as the deploy result reports them (no values a page could not show).
    pub fn summary(&self) -> ServeSummary {
        ServeSummary {
            chat: self.chat.as_ref().map(|chat| chat.access),
            cors_origins: self.cors_origins.clone(),
            conversations: self.conversations,
            monitor: self.monitor,
            devices: self.pairs_devices(),
            approvals: self.monitor || self.approvals || self.expire_approvals,
            expire_approvals: self.expire_approvals,
            push: self.push.is_some(),
            channels: self.channels.clone(),
        }
    }
}

/// What a deployed version serves, in the `result` line of a deploy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServeSummary {
    /// Who may use the chat page, `null` without one.
    pub chat: Option<ChatAccess>,
    /// Sites whose pages may call the agent.
    pub cors_origins: Vec<String>,
    /// The server keeps conversations.
    pub conversations: bool,
    /// The monitoring page is served at `/monitor/`.
    pub monitor: bool,
    /// Devices can be paired (`/v1/devices`).
    pub devices: bool,
    /// The approvals inbox is served (`/v1/approvals`).
    pub approvals: bool,
    /// Requests answer themselves when their time runs out.
    pub expire_approvals: bool,
    /// Web Push notifications are sent.
    pub push: bool,
    /// The messaging channels the agent answers on.
    pub channels: Vec<DirectChannel>,
}

/// The store volume of a deployment, in deploy results and the status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StoreSummary {
    /// Fly volume id (`vol_...`).
    pub volume: String,
    /// Size in GB (what Fly bills).
    pub size_gb: u32,
    /// Region the volume, and so the Machine, lives in.
    pub region: String,
    /// Whether the serving version uses it. `false`: kept from an earlier
    /// deploy and still billed, until a deploy with `--drop-store` deletes it.
    pub in_use: bool,
}

/// `scheme://host[:port]` with an http(s) scheme, or `*`: what `--cors-origin` takes.
fn is_origin(text: &str) -> bool {
    if text == "*" {
        return true;
    }
    let Some(rest) = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")) else { return false };
    !rest.is_empty()
        && rest
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | ':' | '[' | ']'))
        && !rest.starts_with(['.', ':', '-'])
}

/// `mailto:` with an address, or an `https:` URL without spaces.
fn is_push_contact(text: &str) -> bool {
    let printable = !text.chars().any(|character| character.is_control() || character.is_whitespace());
    let mail = text.strip_prefix("mailto:").is_some_and(|address| {
        address.split_once('@').is_some_and(|(name, host)| !name.is_empty() && host.contains('.'))
    });
    let web = text.strip_prefix("https://").is_some_and(|rest| !rest.is_empty());
    printable && text.len() <= 200 && (mail || web)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn everything() -> ServeOptions {
        ServeOptions {
            chat: Some(ChatOptions {
                access: ChatAccess::Guests,
                title: Some("Ask us".into()),
                guest_rate_limit: Some(5),
                guest_budget: Some(60),
            }),
            cors_origins: vec!["https://www.example.com".into()],
            conversations: true,
            monitor: true,
            devices: true,
            approvals: true,
            expire_approvals: true,
            push: Some("mailto:owner@example.com".into()),
            channels: vec![DirectChannel::Telegram, DirectChannel::WhatsApp],
        }
    }

    #[test]
    fn nothing_asked_for_adds_no_flag_and_needs_no_store() {
        let options = ServeOptions::default();
        assert!(!options.needs_store());
        assert!(options.serve_flags("support", "https://support.fly.dev", false).is_empty());
    }

    #[test]
    fn a_chat_page_alone_needs_no_store() {
        let options = ServeOptions { chat: Some(ChatOptions::default()), ..ServeOptions::default() };
        assert!(!options.needs_store(), "guest sessions are signed tokens: nothing is kept");
        assert_eq!(
            options.serve_flags("support", "https://support.fly.dev", false),
            ["--chat", "support", "--chat-access", "key"]
        );
    }

    #[test]
    fn every_option_becomes_the_serve_flag_of_the_same_name() {
        let flags = everything().serve_flags("support", "https://support.fly.dev", true);
        assert_eq!(
            flags,
            [
                "--store",
                "/data",
                "--store-ttl-days",
                "30",
                "--chat",
                "support",
                "--chat-access",
                "guests",
                "--chat-title",
                "Ask us",
                "--chat-guest-rate-limit",
                "5",
                "--chat-guest-budget",
                "60",
                "--cors-origin",
                "https://www.example.com",
                "--conversations",
                "--monitor",
                "--expire-approvals",
                "--push",
                "mailto:owner@example.com",
                "--channel",
                "telegram=support",
                "--channel",
                "whatsapp=support",
                "--public-url",
                "https://support.fly.dev",
            ]
        );
    }

    #[test]
    fn devices_push_and_conversations_each_need_the_store() {
        for options in [
            ServeOptions { monitor: true, ..ServeOptions::default() },
            ServeOptions { devices: true, ..ServeOptions::default() },
            ServeOptions { push: Some("mailto:a@b.co".into()), ..ServeOptions::default() },
            ServeOptions { conversations: true, ..ServeOptions::default() },
            ServeOptions { channels: vec![DirectChannel::WhatsApp], ..ServeOptions::default() },
        ] {
            assert!(options.needs_store(), "{options:?}");
        }
        let inbox_only = ServeOptions { approvals: true, expire_approvals: true, ..ServeOptions::default() };
        assert!(!inbox_only.needs_store(), "the inbox reads the runs: it keeps nothing of its own");
        assert_eq!(inbox_only.serve_flags("a", "https://a.fly.dev", false), ["--expire-approvals"]);
    }

    #[test]
    fn refuses_what_serve_would_refuse_at_startup() {
        let bad_origin = ServeOptions { cors_origins: vec!["www.example.com/shop".into()], ..ServeOptions::default() };
        assert!(bad_origin.validate().unwrap_err().message.contains("--cors-origin"));
        let bad_contact = ServeOptions { push: Some("owner@example.com".into()), ..ServeOptions::default() };
        assert!(bad_contact.validate().unwrap_err().message.contains("mailto:"));
        let bad_title = ServeOptions {
            chat: Some(ChatOptions { title: Some("  ".into()), ..ChatOptions::default() }),
            ..ServeOptions::default()
        };
        assert!(bad_title.validate().is_err());
        assert!(everything().validate().is_ok());
        let any_site = ServeOptions { cors_origins: vec!["*".into()], ..ServeOptions::default() };
        assert!(any_site.validate().is_ok(), "a direct deployment always has an API key");
        let twice = ServeOptions {
            channels: vec![DirectChannel::Telegram, DirectChannel::Telegram],
            ..ServeOptions::default()
        };
        assert!(twice.validate().unwrap_err().message.contains("--channel"));
    }

    #[test]
    fn a_channel_is_served_with_its_kind_and_reads_its_secrets_from_the_environment() {
        let options = ServeOptions { channels: vec![DirectChannel::WhatsApp], ..ServeOptions::default() };
        let flags = options.serve_flags("support", "https://support.fly.dev", true);
        assert_eq!(flags[4..], ["--channel", "whatsapp=support"]);
        assert!(!flags.iter().any(|flag| flag.contains("TOKEN")), "secrets never become flags: {flags:?}");
        assert_eq!(DirectChannel::Telegram.secret_names(), ["TELEGRAM_BOT_TOKEN"]);
        assert_eq!(DirectChannel::WhatsApp.secret_names().len(), 4);
        let summary = serde_json::to_value(options.summary()).unwrap_or_default();
        assert_eq!(summary["channels"], serde_json::json!(["whatsapp"]));
    }

    #[test]
    fn the_summary_says_what_is_on_without_the_contact_address() {
        let summary = serde_json::to_value(everything().summary()).unwrap_or_default();
        assert_eq!(summary["chat"], "guests");
        assert_eq!(summary["push"], true);
        assert_eq!(summary["devices"], true);
        assert!(!summary.to_string().contains("owner@example.com"));
        let none = serde_json::to_value(ServeOptions::default().summary()).unwrap_or_default();
        assert_eq!(none["chat"], serde_json::Value::Null);
        assert_eq!(none["approvals"], false);
    }
}
