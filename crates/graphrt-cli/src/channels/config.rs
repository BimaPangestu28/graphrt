//! What `--channel` and `--channels FILE` configure: which channel talks to
//! which workflow, who may write, and the few sentences the server itself says.
//!
//! Secrets are never written in the file. An entry names the environment
//! variable that holds each one (`token_env`...), read from the process
//! environment or `--env-file`.

use std::path::Path;

use graphrt_channel::{ChannelKind, Secret};
use serde::Deserialize;

use crate::error::CliError;

/// Default environment variables of the Telegram credentials.
pub const TELEGRAM_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";
pub const TELEGRAM_WEBHOOK_SECRET_ENV: &str = "TELEGRAM_WEBHOOK_SECRET";
/// Default environment variables of the WhatsApp credentials.
pub const WHATSAPP_TOKEN_ENV: &str = "WHATSAPP_ACCESS_TOKEN";
pub const WHATSAPP_PHONE_NUMBER_ID_ENV: &str = "WHATSAPP_PHONE_NUMBER_ID";
pub const WHATSAPP_APP_SECRET_ENV: &str = "WHATSAPP_APP_SECRET";
pub const WHATSAPP_VERIFY_TOKEN_ENV: &str = "WHATSAPP_VERIFY_TOKEN";

/// Messages one customer may send per minute, and all customers of a channel together.
pub const DEFAULT_RATE_LIMIT: u32 = 10;
pub const DEFAULT_BUDGET: u32 = 120;
/// Largest file taken from, or sent to, a customer.
pub const DEFAULT_MAX_FILE_MB: u64 = 10;
const MAX_FILE_MB: u64 = 50;

/// How a Telegram bot receives updates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The server asks Telegram and waits (long polling): no public address needed.
    #[default]
    Polling,
    /// Telegram (or Meta) posts to this server: a public HTTPS address is needed.
    Webhook,
}

/// What the server itself says to a customer. Everything else is the workflow's answer.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Messages {
    /// Answer to Telegram's `/start`.
    pub welcome: String,
    /// The channel's budget for all customers is used up.
    pub busy: String,
    /// This customer writes faster than `rate_limit`.
    pub slow_down: String,
    /// The run failed. The reason is never sent.
    pub error: String,
    /// A message with nothing to read (a location, a sticker).
    pub unsupported: String,
    /// A file arrived and the server has no workspace to put it in.
    pub no_files: String,
    pub file_too_large: String,
    /// The run waits for someone of the team.
    pub waiting: String,
    /// The customer wrote again while the run still waits for the team.
    pub still_waiting: String,
    /// The reply to a question was none of its choices.
    pub choose: String,
    /// Answer to Telegram's `/new`.
    pub new_conversation: String,
    /// A question was not answered in time and the run was stopped.
    pub expired: String,
    /// The line above numbered choices.
    pub choice_prompt: String,
}

impl Default for Messages {
    fn default() -> Self {
        let text = str::to_owned;
        Self {
            welcome: text("Hello! Send a message to start."),
            busy: text("We are getting a lot of messages right now. Please try again in a few minutes."),
            slow_down: text("You are sending messages very fast. Please wait a moment and try again."),
            error: text("Sorry, something went wrong on our side. Please try again."),
            unsupported: text("Sorry, I can only read text, photos and documents."),
            no_files: text("Sorry, I cannot take files here. Please write what you need instead."),
            file_too_large: text("Sorry, that file is too large for me to read."),
            waiting: text("One moment please. Someone from our team needs to confirm this."),
            still_waiting: text("We are still waiting for our team. You will get the answer here."),
            choose: text("Please reply with one of the numbers."),
            new_conversation: text("Started a new conversation."),
            expired: text("Nobody answered in time, so this request was closed. Please write again to start over."),
            choice_prompt: text("Reply with a number:"),
        }
    }
}

/// A template message for a WhatsApp answer that is ready after the customer
/// service window closed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub name: String,
    /// E.g. `en_US`, `id`.
    pub language: String,
}

/// One entry of the channels file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    kind: String,
    workflow: String,
    /// The name in URLs, conversation ids and logs. Default: the kind.
    name: Option<String>,
    /// The start input that takes the message. Default: `sys.query`.
    input: Option<String>,
    mode: Option<Mode>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    groups: bool,
    #[serde(default)]
    approvers: Vec<String>,
    rate_limit: Option<u32>,
    budget: Option<u32>,
    max_file_mb: Option<u64>,
    #[serde(default)]
    messages: Messages,
    /// Another API server (a local Bot API server, a fake in tests).
    api_base: Option<String>,
    // Telegram.
    token_env: Option<String>,
    webhook_secret_env: Option<String>,
    // WhatsApp.
    access_token_env: Option<String>,
    phone_number_id: Option<String>,
    phone_number_id_env: Option<String>,
    app_secret_env: Option<String>,
    verify_token_env: Option<String>,
    api_version: Option<String>,
    reengage_template: Option<Template>,
}

/// The channels file: `channels: [...]`, or the list itself.
#[derive(Deserialize)]
#[serde(untagged)]
enum Document {
    Wrapped { channels: Vec<Entry> },
    List(Vec<Entry>),
}

/// The credentials of one channel.
#[derive(Clone, Debug)]
pub enum Credentials {
    Telegram {
        token: Secret,
        /// The secret Telegram sends with each webhook delivery (`mode: webhook`).
        webhook_secret: Option<Secret>,
    },
    WhatsApp {
        access_token: Secret,
        phone_number_id: String,
        app_secret: Secret,
        verify_token: Secret,
        api_version: Option<String>,
        reengage_template: Option<Template>,
    },
}

/// One configured channel.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    pub name: String,
    pub kind: ChannelKind,
    pub workflow: String,
    pub input: Option<String>,
    pub mode: Mode,
    /// Customer ids that may write. Empty: everyone.
    pub allow: Vec<String>,
    /// Answer in group chats too (Telegram).
    pub groups: bool,
    /// Customer ids of the team members who answer approvals in the channel.
    pub approvers: Vec<String>,
    /// Messages per minute of one customer (0 = unlimited).
    pub rate_limit: u32,
    /// Messages per minute of all customers together (0 = unlimited).
    pub budget: u32,
    pub max_file_bytes: u64,
    pub messages: Messages,
    pub api_base: Option<String>,
    pub credentials: Credentials,
}

/// The defaults of `--channel-rate-limit` and `--channel-budget`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Defaults {
    pub rate_limit: u32,
    pub budget: u32,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { rate_limit: DEFAULT_RATE_LIMIT, budget: DEFAULT_BUDGET }
    }
}

/// Whether Telegram accepts `secret` for a webhook: 1 to 256 characters of
/// `A-Z`, `a-z`, `0-9`, `_` and `-`.
fn is_webhook_secret(secret: &str) -> bool {
    (1..=256).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn usage(message: String) -> CliError {
    CliError::usage(message)
}

/// The entry of one `--channel KIND=WORKFLOW[=INPUT]`.
///
/// @throws CliError (usage) - When the value is not of that form.
fn entry_of_flag(spec: &str) -> Result<Entry, CliError> {
    let mut parts = spec.splitn(3, '=');
    let (kind, workflow, input) = (parts.next().unwrap_or_default(), parts.next(), parts.next());
    let Some(workflow) = workflow.filter(|workflow| !workflow.is_empty() && !kind.is_empty()) else {
        return Err(usage(format!(
            "--channel `{spec}`: expected KIND=WORKFLOW, for example `telegram=support` or `whatsapp=support`"
        )));
    };
    Ok(Entry {
        kind: kind.to_owned(),
        workflow: workflow.to_owned(),
        input: input.filter(|input| !input.is_empty()).map(str::to_owned),
        ..Entry::default()
    })
}

/// A secret from the environment variable `name`.
///
/// @throws CliError (usage) - When it is not set. The message names the variable, never a value.
fn secret(channel: &str, what: &str, name: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Secret, CliError> {
    match lookup(name).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty()) {
        Some(value) => Ok(Secret::new(value)),
        None => Err(usage(format!("channel `{channel}`: set ${name} ({what})"))),
    }
}

fn resolve(
    entry: Entry,
    defaults: Defaults,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<ChannelConfig, CliError> {
    let Some(kind) = ChannelKind::parse(&entry.kind) else {
        return Err(usage(format!("channel kind `{}` is not known: use `telegram` or `whatsapp`", entry.kind)));
    };
    let name = entry.name.unwrap_or_else(|| kind.as_str().to_owned());
    // The name is part of URLs, conversation ids and file paths.
    let named = !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !named {
        return Err(usage(format!("channel name `{name}`: use up to 32 letters, digits and `_`")));
    }
    let env = |given: &Option<String>, default: &str| given.clone().unwrap_or_else(|| default.to_owned());
    let (credentials, mode) = match kind {
        ChannelKind::Telegram => {
            let token =
                secret(&name, "the bot token from @BotFather", &env(&entry.token_env, TELEGRAM_TOKEN_ENV), lookup)?;
            let secret_env = env(&entry.webhook_secret_env, TELEGRAM_WEBHOOK_SECRET_ENV);
            let webhook_secret = lookup(&secret_env).filter(|value| !value.trim().is_empty());
            // With the flag alone, a webhook secret in the environment chooses webhook mode.
            let mode = entry.mode.unwrap_or(if webhook_secret.is_some() { Mode::Webhook } else { Mode::Polling });
            let webhook_secret = match (mode, webhook_secret) {
                (Mode::Polling, _) => None,
                (Mode::Webhook, Some(value)) => {
                    let value = value.trim().to_owned();
                    if !is_webhook_secret(&value) {
                        return Err(usage(format!(
                            "channel `{name}`: ${secret_env} must be 1 to 256 characters of A-Z, a-z, 0-9, _ and -"
                        )));
                    }
                    Some(Secret::new(value))
                }
                (Mode::Webhook, None) => {
                    return Err(usage(format!(
                        "channel `{name}`: webhook mode needs ${secret_env} (any long random text of letters and digits)"
                    )));
                }
            };
            (Credentials::Telegram { token, webhook_secret }, mode)
        }
        ChannelKind::WhatsApp => {
            if entry.mode == Some(Mode::Polling) {
                return Err(usage(format!("channel `{name}`: WhatsApp only delivers by webhook")));
            }
            let id_env = env(&entry.phone_number_id_env, WHATSAPP_PHONE_NUMBER_ID_ENV);
            let phone_number_id = entry
                .phone_number_id
                .clone()
                .or_else(|| lookup(&id_env))
                .map(|id| id.trim().to_owned())
                .filter(|id| !id.is_empty())
                .ok_or_else(|| usage(format!("channel `{name}`: set ${id_env} (the phone number id of the app)")))?;
            let credentials = Credentials::WhatsApp {
                access_token: secret(
                    &name,
                    "the access token",
                    &env(&entry.access_token_env, WHATSAPP_TOKEN_ENV),
                    lookup,
                )?,
                phone_number_id,
                app_secret: secret(
                    &name,
                    "the app secret, which signs every delivery",
                    &env(&entry.app_secret_env, WHATSAPP_APP_SECRET_ENV),
                    lookup,
                )?,
                verify_token: secret(
                    &name,
                    "the verify token you chose for the webhook",
                    &env(&entry.verify_token_env, WHATSAPP_VERIFY_TOKEN_ENV),
                    lookup,
                )?,
                api_version: entry.api_version.clone(),
                reengage_template: entry.reengage_template.clone(),
            };
            (credentials, Mode::Webhook)
        }
        _ => return Err(usage(format!("channel kind `{}` is not built into this binary", entry.kind))),
    };
    if entry.groups && kind != ChannelKind::Telegram {
        return Err(usage(format!("channel `{name}`: `groups` is for Telegram, WhatsApp groups are not read")));
    }
    let max_file_mb = entry.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB);
    if !(1..=MAX_FILE_MB).contains(&max_file_mb) {
        return Err(usage(format!("channel `{name}`: `max_file_mb` is 1 to {MAX_FILE_MB}")));
    }
    Ok(ChannelConfig {
        name,
        kind,
        workflow: entry.workflow,
        input: entry.input,
        mode,
        allow: entry.allow,
        groups: entry.groups,
        approvers: entry.approvers,
        rate_limit: entry.rate_limit.unwrap_or(defaults.rate_limit),
        budget: entry.budget.unwrap_or(defaults.budget),
        max_file_bytes: max_file_mb * 1024 * 1024,
        messages: entry.messages,
        api_base: entry.api_base,
        credentials,
    })
}

/// The channels of `--channel` flags and of the `--channels` file, with their
/// secrets read through `lookup`.
///
/// @throws CliError (usage) - On a malformed flag or file, an unknown kind, a
///   missing secret, or two channels with one name.
pub fn load(
    flags: &[String],
    file: Option<&Path>,
    defaults: Defaults,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<ChannelConfig>, CliError> {
    let mut entries = flags.iter().map(|spec| entry_of_flag(spec)).collect::<Result<Vec<_>, _>>()?;
    if let Some(path) = file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| usage(format!("cannot read channels file `{}`: {e}", path.display())))?;
        let value = crate::setup::parse_structured(&text, "channels file")?;
        let document: Document =
            serde_json::from_value(value).map_err(|e| usage(format!("channels file `{}`: {e}", path.display())))?;
        entries.extend(match document {
            Document::Wrapped { channels } | Document::List(channels) => channels,
        });
    }
    let mut channels: Vec<ChannelConfig> = Vec::with_capacity(entries.len());
    for entry in entries {
        let channel = resolve(entry, defaults, lookup)?;
        if channels.iter().any(|other| other.name == channel.name) {
            return Err(usage(format!(
                "two channels are named `{}`: give one of them another `name` in the channels file",
                channel.name
            )));
        }
        channels.push(channel);
    }
    Ok(channels)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
    }

    fn load_flags(flags: &[&str], vars: &[(&str, &str)]) -> Result<Vec<ChannelConfig>, CliError> {
        let flags: Vec<String> = flags.iter().map(|flag| (*flag).to_owned()).collect();
        load(&flags, None, Defaults::default(), &env(vars))
    }

    #[test]
    fn one_flag_and_one_variable_configure_telegram_by_polling() {
        let channels = load_flags(&["telegram=support"], &[(TELEGRAM_TOKEN_ENV, "123456:token")]).unwrap();
        let channel = &channels[0];
        assert_eq!((channel.name.as_str(), channel.workflow.as_str()), ("telegram", "support"));
        assert_eq!(
            (channel.kind, channel.mode, channel.input.as_deref()),
            (ChannelKind::Telegram, Mode::Polling, None)
        );
        assert_eq!((channel.rate_limit, channel.budget), (DEFAULT_RATE_LIMIT, DEFAULT_BUDGET));
        assert!(!channel.groups && channel.allow.is_empty());
        let with_input = load_flags(&["telegram=support=message"], &[(TELEGRAM_TOKEN_ENV, "123456:token")]).unwrap();
        assert_eq!(with_input[0].input.as_deref(), Some("message"));
    }

    #[test]
    fn a_webhook_secret_in_the_environment_chooses_webhook_mode() {
        let vars = [(TELEGRAM_TOKEN_ENV, "123456:token"), (TELEGRAM_WEBHOOK_SECRET_ENV, "long_random-secret")];
        let channels = load_flags(&["telegram=support"], &vars).unwrap();
        assert_eq!(channels[0].mode, Mode::Webhook);
        let bad = [(TELEGRAM_TOKEN_ENV, "123456:token"), (TELEGRAM_WEBHOOK_SECRET_ENV, "has space")];
        assert!(load_flags(&["telegram=support"], &bad).is_err());
    }

    #[test]
    fn whatsapp_needs_all_four_values_and_says_which_is_missing() {
        let all = [
            (WHATSAPP_TOKEN_ENV, "EAAG-token"),
            (WHATSAPP_PHONE_NUMBER_ID_ENV, "1065"),
            (WHATSAPP_APP_SECRET_ENV, "app-secret"),
            (WHATSAPP_VERIFY_TOKEN_ENV, "verify-me"),
        ];
        let channels = load_flags(&["whatsapp=support"], &all).unwrap();
        assert_eq!((channels[0].kind, channels[0].mode), (ChannelKind::WhatsApp, Mode::Webhook));
        for missing in 0..all.len() {
            let mut vars = all.to_vec();
            let (name, _) = vars.remove(missing);
            let error = load_flags(&["whatsapp=support"], &vars).unwrap_err().to_string();
            assert!(error.contains(name), "{error}");
            assert!(!error.contains("EAAG-token") && !error.contains("app-secret"), "{error}");
        }
    }

    #[test]
    fn malformed_flags_and_unknown_kinds_are_refused() {
        let vars = [(TELEGRAM_TOKEN_ENV, "123456:token")];
        for bad in ["telegram", "telegram=", "=support", "sms=support"] {
            assert!(load_flags(&[bad], &vars).is_err(), "{bad}");
        }
        assert!(load_flags(&["telegram=support"], &[]).unwrap_err().to_string().contains(TELEGRAM_TOKEN_ENV));
        let twice = load_flags(&["telegram=a", "telegram=b"], &vars).unwrap_err().to_string();
        assert!(twice.contains("two channels are named"), "{twice}");
    }

    #[test]
    fn a_file_sets_what_the_flag_cannot() {
        let dir = std::env::temp_dir().join(format!("graphrt-channels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("channels.yaml");
        let yaml = r#"
channels:
  - kind: telegram
    name: shop
    workflow: support
    token_env: SHOP_TOKEN
    allow: ["42"]
    groups: true
    approvers: ["7"]
    rate_limit: 3
    max_file_mb: 5
    messages:
      welcome: "Halo! Ada yang bisa dibantu?"
  - kind: whatsapp
    workflow: support
    phone_number_id: "1065"
    reengage_template: { name: follow_up, language: id }
"#;
        std::fs::write(&path, yaml).unwrap();
        let vars = [
            ("SHOP_TOKEN", "123456:token"),
            (WHATSAPP_TOKEN_ENV, "t"),
            (WHATSAPP_APP_SECRET_ENV, "s"),
            (WHATSAPP_VERIFY_TOKEN_ENV, "v"),
        ];
        let channels = load(&[], Some(&path), Defaults { rate_limit: 10, budget: 50 }, &env(&vars)).unwrap();
        let shop = &channels[0];
        assert_eq!((shop.name.as_str(), shop.rate_limit, shop.budget), ("shop", 3, 50));
        assert_eq!((shop.allow.as_slice(), shop.approvers.as_slice()), (&["42".to_owned()][..], &["7".to_owned()][..]));
        assert!(shop.groups);
        assert_eq!(shop.max_file_bytes, 5 * 1024 * 1024);
        assert_eq!(shop.messages.welcome, "Halo! Ada yang bisa dibantu?");
        assert_eq!(shop.messages.error, Messages::default().error, "unset messages keep their default");
        let Credentials::WhatsApp { reengage_template, phone_number_id, .. } = &channels[1].credentials else {
            panic!("expected WhatsApp credentials")
        };
        assert_eq!(phone_number_id, "1065");
        assert_eq!(reengage_template.as_ref().map(|t| t.name.as_str()), Some("follow_up"));
        std::fs::write(&path, "channels:\n  - kind: telegram\n    workflow: s\n    tokn_env: X\n").unwrap();
        assert!(load(&[], Some(&path), Defaults::default(), &env(&vars)).is_err(), "unknown keys are refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
