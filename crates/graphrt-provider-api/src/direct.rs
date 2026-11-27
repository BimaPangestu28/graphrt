//! What every direct deploy (`graphrt deploy --direct`, one agent served
//! straight by a platform with no gateway) shares, whatever the platform: the
//! `graphrt serve` features it turns on besides the API (chat page,
//! conversations, monitoring from a phone, messaging channels: `docs/web.md`,
//! `docs/monitoring.md`, `docs/channels.md`, each off unless asked for), the
//! agent file it ships and the progress events it reports.

use serde::Serialize;

/// Where the store is mounted in the container (`graphrt serve --store`).
pub const STORE_PATH: &str = "/data";
/// Days a finished run stays in the store (`graphrt serve --store-ttl-days`),
/// so the store does not fill up with run records.
pub const STORE_RUN_TTL_DAYS: u32 = 30;

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
/// Its credentials are platform secrets of the names `graphrt serve` reads, never flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DirectChannel {
    /// Telegram, by long polling: needs an instance that keeps running.
    Telegram,
    /// WhatsApp Cloud API: Meta posts to `<public url>/v1/channels/whatsapp/webhook`.
    WhatsApp,
    /// Slack, by the Events API: Slack posts to `<public url>/v1/channels/slack/webhook`.
    /// With `SLACK_APP_TOKEN` as well it uses Socket Mode, which needs an instance that keeps running.
    Slack,
    /// Microsoft Teams: the Azure Bot posts to `<public url>/v1/channels/teams/webhook`.
    Teams,
}

impl DirectChannel {
    /// The channel named `kind` (`telegram`, `whatsapp`, `slack`, `teams`).
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "telegram" => Some(Self::Telegram),
            "whatsapp" => Some(Self::WhatsApp),
            "slack" => Some(Self::Slack),
            "teams" => Some(Self::Teams),
            _ => None,
        }
    }

    /// The `KIND` of `--channel KIND=WORKFLOW`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::WhatsApp => "whatsapp",
            Self::Slack => "slack",
            Self::Teams => "teams",
        }
    }

    /// The secrets `graphrt serve` reads for this channel (docs/channels.md,
    /// docs/slack-teams.md). Slack's Socket Mode reads `SLACK_APP_TOKEN` instead
    /// of the signing secret.
    pub fn secret_names(self) -> &'static [&'static str] {
        match self {
            Self::Telegram => &["TELEGRAM_BOT_TOKEN"],
            Self::WhatsApp => {
                &["WHATSAPP_ACCESS_TOKEN", "WHATSAPP_PHONE_NUMBER_ID", "WHATSAPP_APP_SECRET", "WHATSAPP_VERIFY_TOKEN"]
            }
            Self::Slack => &["SLACK_BOT_TOKEN", "SLACK_SIGNING_SECRET"],
            Self::Teams => &["TEAMS_APP_ID", "TEAMS_APP_PASSWORD", "TEAMS_TENANT_ID"],
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
    /// Whether the deployment keeps state that must outlive an instance stop or a
    /// redeploy: paired devices, push subscriptions with the server's push key,
    /// conversations, or channel conversations (with the message ids that keep a
    /// redelivered message from running twice). Such a deployment gets persistent storage as its `--store`.
    pub fn needs_store(&self) -> bool {
        self.monitor || self.devices || self.push.is_some() || self.conversations || !self.channels.is_empty()
    }

    /// Whether a device is ever paired (then the pairing link needs the public address).
    fn pairs_devices(&self) -> bool {
        self.monitor || self.devices
    }

    /// Checks the options before any call to the platform.
    ///
    /// @throws String - values `graphrt serve` would refuse at startup, as a message for the user
    pub fn validate(&self) -> Result<(), String> {
        if let Some(chat) = &self.chat
            && let Some(title) = &chat.title
            && (title.trim().is_empty() || title.len() > 120 || title.chars().any(char::is_control))
        {
            return Err("--chat-title must be 1-120 printable characters".to_owned());
        }
        for origin in &self.cors_origins {
            if !is_origin(origin) {
                return Err(format!(
                    "--cors-origin {origin:?} is not an origin such as https://www.example.com (scheme and host, no path)"
                ));
            }
        }
        if let Some(contact) = &self.push
            && !is_push_contact(contact)
        {
            return Err(format!(
                "--push {contact:?} must be an address push services may write to: mailto:you@example.com or an https: URL"
            ));
        }
        let distinct: std::collections::BTreeSet<DirectChannel> = self.channels.iter().copied().collect();
        if distinct.len() != self.channels.len() {
            return Err("--channel names each kind once".to_owned());
        }
        Ok(())
    }

    /// The flags added to `graphrt serve <dir> --addr ...` for these options.
    ///
    /// @param workflow - the name the agent is served under (&str)
    /// @param public_url - the deployment's public address, for pairing links (&str)
    /// @param with_store - persistent storage is mounted at [`STORE_PATH`] (bool)
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
            channels: self.channels.iter().map(|channel| channel.as_str()).collect(),
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
    /// The messaging channels the agent answers on (`telegram`, `whatsapp`).
    pub channels: Vec<&'static str>,
}

/// How a direct deployment runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceMode {
    /// One instance always running.
    AlwaysOn,
    /// Stopped when idle, started again by the platform on the next request.
    ScaleToZero,
}

impl ServiceMode {
    /// The name used on the command line and in metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysOn => "always-on",
            Self::ScaleToZero => "scale-to-zero",
        }
    }

    /// Parses [`ServiceMode::as_str`].
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "always-on" => Some(Self::AlwaysOn),
            "scale-to-zero" => Some(Self::ScaleToZero),
            _ => None,
        }
    }
}

/// The agent a direct deploy ships: a `.grt` bundle or a workflow file.
#[derive(Clone)]
pub struct AgentFile {
    /// File name, e.g. `support.grt` or `support.yaml`; `graphrt serve` names
    /// the workflow after its stem ([`workflow_name_of`]).
    pub file_name: String,
    /// File contents.
    pub contents: Vec<u8>,
    /// Bytes the agent takes once unpacked: a bundle's files (knowledge stores
    /// compress well), else the file's size. Sizes the instance's memory.
    pub unpacked_bytes: u64,
}

impl AgentFile {
    /// An agent file whose unpacked size is its own size (see [`AgentFile::unpacked`]).
    pub fn new(file_name: impl Into<String>, contents: Vec<u8>) -> Self {
        let unpacked_bytes = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        Self { file_name: file_name.into(), contents, unpacked_bytes }
    }

    /// The same file, `bytes` once unpacked (never less than the file itself).
    pub fn unpacked(mut self, bytes: u64) -> Self {
        self.unpacked_bytes = bytes.max(self.unpacked_bytes);
        self
    }
}

impl std::fmt::Debug for AgentFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentFile")
            .field("file_name", &self.file_name)
            .field("bytes", &self.contents.len())
            .field("unpacked_bytes", &self.unpacked_bytes)
            .finish()
    }
}

/// The phases a deploy reports progress for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Input checks and the platform resources the deployment needs.
    Preparing,
    /// The platform's secrets and the API key.
    Secrets,
    /// Creating or updating the version and booting it.
    Starting,
    /// Waiting for `/readyz`, then promoting the version.
    Checking,
}

/// State of a step in a progress event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Done,
    Failed,
}

/// One progress event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepEvent {
    pub step: Step,
    pub status: StepStatus,
    pub message: String,
}

/// The workflow name `graphrt serve` gives an agent file: its stem, with
/// everything but ASCII letters, digits and `_` turned into `-`.
pub fn workflow_name_of(file_name: &str) -> String {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(stem, _)| stem);
    let slug: String = stem
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || character == '_' { character } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() { "agent".to_owned() } else { slug }
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
    fn modes_round_trip() {
        for mode in [ServiceMode::AlwaysOn, ServiceMode::ScaleToZero] {
            assert_eq!(ServiceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ServiceMode::parse("sometimes"), None);
    }

    #[test]
    fn workflow_names_follow_the_file_stem() {
        assert_eq!(workflow_name_of("Support Bot.GRT"), "Support-Bot");
        assert_eq!(workflow_name_of("support.yaml"), "support");
        assert_eq!(workflow_name_of(".yaml"), "agent");
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
        assert!(bad_origin.validate().unwrap_err().contains("--cors-origin"));
        let bad_contact = ServeOptions { push: Some("owner@example.com".into()), ..ServeOptions::default() };
        assert!(bad_contact.validate().unwrap_err().contains("mailto:"));
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
        assert!(twice.validate().unwrap_err().contains("--channel"));
    }

    #[test]
    fn a_channel_is_served_with_its_kind_and_reads_its_secrets_from_the_environment() {
        let options = ServeOptions { channels: vec![DirectChannel::WhatsApp], ..ServeOptions::default() };
        let flags = options.serve_flags("support", "https://support.fly.dev", true);
        assert_eq!(flags[4..], ["--channel", "whatsapp=support"]);
        assert!(!flags.iter().any(|flag| flag.contains("TOKEN")), "secrets never become flags: {flags:?}");
        assert_eq!(DirectChannel::Telegram.secret_names(), ["TELEGRAM_BOT_TOKEN"]);
        assert_eq!(DirectChannel::WhatsApp.secret_names().len(), 4);
        for kind in ["slack", "teams"] {
            let channel = DirectChannel::parse(kind).unwrap_or(DirectChannel::Telegram);
            assert_eq!(channel.as_str(), kind);
            assert!(!channel.secret_names().is_empty());
        }
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
