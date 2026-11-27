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
/// Default environment variables of the Slack credentials.
pub const SLACK_BOT_TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
pub const SLACK_APP_TOKEN_ENV: &str = "SLACK_APP_TOKEN";
pub const SLACK_SIGNING_SECRET_ENV: &str = "SLACK_SIGNING_SECRET";
/// Default environment variables of the Teams (Azure Bot) credentials.
pub const TEAMS_APP_ID_ENV: &str = "TEAMS_APP_ID";
pub const TEAMS_APP_PASSWORD_ENV: &str = "TEAMS_APP_PASSWORD";
pub const TEAMS_TENANT_ID_ENV: &str = "TEAMS_TENANT_ID";

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
    /// Telegram, Meta, Slack or Teams posts to this server: a public HTTPS address is needed.
    Webhook,
    /// Slack Socket Mode: the server opens a WebSocket to Slack, no public address needed.
    Socket,
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

/// The language of the server's own sentences ([`Messages`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// English.
    #[default]
    En,
    /// Indonesian.
    Id,
}

impl Language {
    /// The language named `name` (`en`, `id`).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "en" => Some(Self::En),
            "id" => Some(Self::Id),
            _ => None,
        }
    }
}

impl Messages {
    /// The built-in sentences of `language`.
    pub fn of(language: Language) -> Self {
        match language {
            Language::En => Self::default(),
            Language::Id => Self::indonesian(),
        }
    }

    fn indonesian() -> Self {
        let text = str::to_owned;
        Self {
            welcome: text("Halo! Silakan kirim pesan untuk mulai."),
            busy: text("Kami sedang menerima banyak pesan. Silakan coba lagi beberapa menit lagi."),
            slow_down: text("Pesan Anda terlalu cepat. Mohon tunggu sebentar lalu coba lagi."),
            error: text("Maaf, ada kendala di sisi kami. Silakan coba lagi."),
            unsupported: text("Maaf, saya hanya bisa membaca teks, foto, dan dokumen."),
            no_files: text("Maaf, saya tidak bisa menerima file di sini. Silakan tulis kebutuhan Anda."),
            file_too_large: text("Maaf, file itu terlalu besar untuk saya baca."),
            waiting: text("Mohon tunggu sebentar. Tim kami perlu mengonfirmasi ini."),
            still_waiting: text("Kami masih menunggu tim kami. Jawabannya akan dikirim di sini."),
            choose: text("Silakan balas dengan salah satu nomor."),
            new_conversation: text("Percakapan baru dimulai."),
            expired: text(
                "Tidak ada yang menjawab tepat waktu, jadi permintaan ini ditutup. Silakan tulis lagi untuk mulai dari awal.",
            ),
            choice_prompt: text("Balas dengan nomor:"),
        }
    }
}

/// The `messages` of a channels file entry: the sentences it replaces.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MessageOverrides {
    welcome: Option<String>,
    busy: Option<String>,
    slow_down: Option<String>,
    error: Option<String>,
    unsupported: Option<String>,
    no_files: Option<String>,
    file_too_large: Option<String>,
    waiting: Option<String>,
    still_waiting: Option<String>,
    choose: Option<String>,
    new_conversation: Option<String>,
    expired: Option<String>,
    choice_prompt: Option<String>,
}

impl MessageOverrides {
    /// `base` with these sentences put in.
    fn apply(self, mut base: Messages) -> Messages {
        let pairs: [(Option<String>, &mut String); 13] = [
            (self.welcome, &mut base.welcome),
            (self.busy, &mut base.busy),
            (self.slow_down, &mut base.slow_down),
            (self.error, &mut base.error),
            (self.unsupported, &mut base.unsupported),
            (self.no_files, &mut base.no_files),
            (self.file_too_large, &mut base.file_too_large),
            (self.waiting, &mut base.waiting),
            (self.still_waiting, &mut base.still_waiting),
            (self.choose, &mut base.choose),
            (self.new_conversation, &mut base.new_conversation),
            (self.expired, &mut base.expired),
            (self.choice_prompt, &mut base.choice_prompt),
        ];
        for (given, slot) in pairs {
            if let Some(given) = given {
                *slot = given;
            }
        }
        base
    }
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
    /// Empty for a channel that only sends notifications (`notify`).
    #[serde(default)]
    workflow: String,
    /// The name in URLs, conversation ids and logs. Default: the kind.
    name: Option<String>,
    /// The start input that takes the message. Default: `sys.query`.
    input: Option<String>,
    mode: Option<Mode>,
    #[serde(default)]
    allow: Vec<String>,
    groups: Option<bool>,
    #[serde(default)]
    approvers: Vec<String>,
    /// Where notices of failed runs and pending approvals go.
    #[serde(default)]
    notify: Vec<String>,
    rate_limit: Option<u32>,
    budget: Option<u32>,
    max_file_mb: Option<u64>,
    /// The language of the server's own sentences. Default: `--channel-language`.
    language: Option<Language>,
    #[serde(default)]
    messages: MessageOverrides,
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
    // Slack (`token_env` is the bot token).
    app_token_env: Option<String>,
    signing_secret_env: Option<String>,
    // Teams.
    app_id: Option<String>,
    app_id_env: Option<String>,
    app_password_env: Option<String>,
    tenant_id: Option<String>,
    tenant_id_env: Option<String>,
    /// Another token server (a fake in tests).
    login_base: Option<String>,
    /// Another OpenID configuration for the keys that sign deliveries (a fake in tests).
    openid_configuration: Option<String>,
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
    Slack {
        /// `xoxb-...`: everything the app sends.
        bot_token: Secret,
        /// `xapp-...`: Socket Mode (`mode: socket`).
        app_token: Option<Secret>,
        /// The secret Slack signs Events API deliveries with (`mode: webhook`).
        signing_secret: Option<Secret>,
    },
    Teams {
        /// The Microsoft App ID of the Azure Bot.
        app_id: String,
        /// Its client secret.
        app_password: Secret,
        /// The Entra tenant of a single tenant bot.
        tenant_id: Option<String>,
        login_base: Option<String>,
        openid_configuration: Option<String>,
    },
}

/// One configured channel.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    pub name: String,
    pub kind: ChannelKind,
    /// The workflow that answers. Empty for a channel that only sends notifications.
    pub workflow: String,
    pub input: Option<String>,
    pub mode: Mode,
    /// Customer ids that may write. Empty: everyone.
    pub allow: Vec<String>,
    /// Answer in group chats too (Telegram).
    pub groups: bool,
    /// Customer ids of the team members who answer approvals in the channel.
    pub approvers: Vec<String>,
    /// Who hears about failed runs and pending approvals of any run: chats,
    /// channels or people of this channel. Empty: nobody.
    pub notify: Vec<String>,
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
    /// `--channel-language`.
    pub language: Language,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { rate_limit: DEFAULT_RATE_LIMIT, budget: DEFAULT_BUDGET, language: Language::En }
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
        return Err(usage(format!(
            "channel kind `{}` is not known: use `telegram`, `whatsapp`, `slack` or `teams`",
            entry.kind
        )));
    };
    let name = entry.name.clone().unwrap_or_else(|| kind.as_str().to_owned());
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
                (Mode::Socket, _) => {
                    return Err(usage(format!("channel `{name}`: Telegram takes `mode: polling` or `webhook`")));
                }
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
        ChannelKind::Slack => slack_credentials(&entry, &name, lookup)?,
        ChannelKind::Teams => teams_credentials(&entry, &name, lookup)?,
        _ => return Err(usage(format!("channel kind `{}` is not built into this binary", entry.kind))),
    };
    // Slack and Teams answer mentions in channels: that is how a team uses them.
    let groups = match kind {
        ChannelKind::Slack | ChannelKind::Teams => entry.groups.unwrap_or(true),
        ChannelKind::Telegram => entry.groups.unwrap_or(false),
        _ if entry.groups == Some(true) => {
            return Err(usage(format!("channel `{name}`: `groups` is for Telegram, WhatsApp groups are not read")));
        }
        _ => false,
    };
    if entry.workflow.is_empty() && entry.notify.is_empty() {
        return Err(usage(format!(
            "channel `{name}`: set `workflow` (who answers), or `notify` for a channel that only sends notifications"
        )));
    }
    if !entry.notify.is_empty() && kind == ChannelKind::WhatsApp {
        return Err(usage(format!(
            "channel `{name}`: WhatsApp takes no messages a customer did not ask for, so it cannot `notify`"
        )));
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
        groups,
        approvers: entry.approvers,
        notify: entry.notify,
        rate_limit: entry.rate_limit.unwrap_or(defaults.rate_limit),
        budget: entry.budget.unwrap_or(defaults.budget),
        max_file_bytes: max_file_mb * 1024 * 1024,
        messages: entry.messages.apply(Messages::of(entry.language.unwrap_or(defaults.language))),
        api_base: entry.api_base,
        credentials,
    })
}

/// The start input a customer's message goes to, checked against the workflow's start
/// inputs (`declared`, `None` when the workflow is not served) and the ones a run cannot
/// start without (`required`). Without `input`, a workflow whose start requires exactly one
/// input gets the message there, so `--channel telegram=support` works for a workflow that
/// asks for `message`. `None`: `sys.query`.
///
/// @throws CliError (usage) - An input the workflow does not declare, or required inputs a
///   message cannot fill: the server would answer every customer "something went wrong".
pub fn message_input(
    config: &ChannelConfig,
    required: &[String],
    declared: Option<Vec<String>>,
) -> Result<Option<String>, CliError> {
    let Some(declared) = declared else { return Ok(config.input.clone()) };
    let (name, workflow) = (&config.name, &config.workflow);
    let chosen = match (&config.input, required) {
        (Some(input), _) => {
            if !declared.contains(input) {
                return Err(usage(format!(
                    "channel `{name}`: workflow `{workflow}` declares no start input `{input}`"
                )));
            }
            input.clone()
        }
        (None, []) => return Ok(None),
        (None, [only]) => only.clone(),
        (None, several) => {
            return Err(usage(format!(
                "channel `{name}`: workflow `{workflow}` requires the start inputs {}; a message fills one: name it \
                 with `{}={workflow}=INPUT` and give the others a default",
                several.join(", "),
                config.kind.as_str()
            )));
        }
    };
    let unfilled: Vec<&str> = required.iter().map(String::as_str).filter(|input| *input != chosen).collect();
    if !unfilled.is_empty() {
        return Err(usage(format!(
            "channel `{name}`: workflow `{workflow}` also requires {}, which a customer's message cannot fill: give \
             it a default",
            unfilled.join(", ")
        )));
    }
    Ok(Some(chosen))
}

/// A value from the file, else from the environment variable `env_name`.
fn value_or_env(given: &Option<String>, env_name: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    given.clone().or_else(|| lookup(env_name)).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

/// The Slack credentials and mode of `entry`: Socket Mode when an app token is
/// there (no public address needed), else the Events API with its signing secret.
fn slack_credentials(
    entry: &Entry,
    name: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(Credentials, Mode), CliError> {
    let env = |given: &Option<String>, default: &str| given.clone().unwrap_or_else(|| default.to_owned());
    let bot_token = secret(
        name,
        "the Bot User OAuth Token, xoxb-..., from OAuth & Permissions of the Slack app",
        &env(&entry.token_env, SLACK_BOT_TOKEN_ENV),
        lookup,
    )?;
    let app_env = env(&entry.app_token_env, SLACK_APP_TOKEN_ENV);
    let signing_env = env(&entry.signing_secret_env, SLACK_SIGNING_SECRET_ENV);
    let app_token = lookup(&app_env).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
    let signing_secret = lookup(&signing_env).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
    let mode = entry.mode.unwrap_or(if app_token.is_some() { Mode::Socket } else { Mode::Webhook });
    let (app_token, signing_secret) = match mode {
        Mode::Socket => match app_token {
            Some(token) => (Some(Secret::new(token)), None),
            None => {
                return Err(usage(format!(
                    "channel `{name}`: Socket Mode needs ${app_env} (an app level token, xapp-..., with \
                     connections:write, from Basic Information of the Slack app)"
                )));
            }
        },
        Mode::Webhook => match signing_secret {
            Some(secret) => (None, Some(Secret::new(secret))),
            None => {
                return Err(usage(format!(
                    "channel `{name}`: set ${app_env} for Socket Mode (no public address needed), or \
                     ${signing_env} (the Signing Secret from Basic Information) for the Events API"
                )));
            }
        },
        Mode::Polling => return Err(usage(format!("channel `{name}`: Slack takes `mode: socket` or `webhook`"))),
    };
    Ok((Credentials::Slack { bot_token, app_token, signing_secret }, mode))
}

/// The Teams credentials of `entry`. Teams always posts to the server (an
/// Azure Bot's messaging endpoint).
fn teams_credentials(
    entry: &Entry,
    name: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(Credentials, Mode), CliError> {
    if matches!(entry.mode, Some(Mode::Polling | Mode::Socket)) {
        return Err(usage(format!("channel `{name}`: Teams only delivers to the bot's messaging endpoint (webhook)")));
    }
    let id_env = entry.app_id_env.clone().unwrap_or_else(|| TEAMS_APP_ID_ENV.to_owned());
    let app_id = value_or_env(&entry.app_id, &id_env, lookup)
        .ok_or_else(|| usage(format!("channel `{name}`: set ${id_env} (the Microsoft App ID of the Azure Bot)")))?;
    let app_password = secret(
        name,
        "the client secret of the Azure Bot's app",
        &entry.app_password_env.clone().unwrap_or_else(|| TEAMS_APP_PASSWORD_ENV.to_owned()),
        lookup,
    )?;
    let tenant_env = entry.tenant_id_env.clone().unwrap_or_else(|| TEAMS_TENANT_ID_ENV.to_owned());
    let tenant_id = value_or_env(&entry.tenant_id, &tenant_env, lookup);
    let credentials = Credentials::Teams {
        app_id,
        app_password,
        tenant_id,
        login_base: entry.login_base.clone(),
        openid_configuration: entry.openid_configuration.clone(),
    };
    Ok((credentials, Mode::Webhook))
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
    fn slack_picks_socket_mode_with_an_app_token_and_the_events_api_with_a_signing_secret() {
        let socket = [(SLACK_BOT_TOKEN_ENV, "xoxb-1-2-abc"), (SLACK_APP_TOKEN_ENV, "xapp-1-A-2-abc")];
        let channels = load_flags(&["slack=support"], &socket).unwrap();
        assert_eq!((channels[0].kind, channels[0].mode), (ChannelKind::Slack, Mode::Socket));
        assert!(channels[0].groups, "mentions in channels are answered");
        let events = [(SLACK_BOT_TOKEN_ENV, "xoxb-1-2-abc"), (SLACK_SIGNING_SECRET_ENV, "signing")];
        assert_eq!(load_flags(&["slack=support"], &events).unwrap()[0].mode, Mode::Webhook);
        let error = load_flags(&["slack=support"], &[(SLACK_BOT_TOKEN_ENV, "xoxb-1-2-abc")]).unwrap_err().to_string();
        assert!(error.contains(SLACK_APP_TOKEN_ENV) && error.contains(SLACK_SIGNING_SECRET_ENV), "{error}");
        let error = load_flags(&["slack=support"], &[(SLACK_APP_TOKEN_ENV, "xapp-1")]).unwrap_err().to_string();
        assert!(error.contains(SLACK_BOT_TOKEN_ENV), "{error}");
    }

    #[test]
    fn teams_needs_its_app_id_and_secret_and_takes_a_tenant() {
        let vars = [
            (TEAMS_APP_ID_ENV, "00000000-1111-2222-3333-444444444444"),
            (TEAMS_APP_PASSWORD_ENV, "client-secret"),
            (TEAMS_TENANT_ID_ENV, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        ];
        let channels = load_flags(&["teams=support"], &vars).unwrap();
        assert_eq!((channels[0].kind, channels[0].mode), (ChannelKind::Teams, Mode::Webhook));
        let Credentials::Teams { tenant_id, .. } = &channels[0].credentials else {
            panic!("expected Teams credentials")
        };
        assert_eq!(tenant_id.as_deref(), Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        for missing in [TEAMS_APP_ID_ENV, TEAMS_APP_PASSWORD_ENV] {
            let rest: Vec<_> = vars.iter().filter(|(name, _)| *name != missing).copied().collect();
            let error = load_flags(&["teams=support"], &rest).unwrap_err().to_string();
            assert!(error.contains(missing) && !error.contains("client-secret"), "{error}");
        }
    }

    #[test]
    fn a_channel_without_a_workflow_only_notifies_and_whatsapp_cannot() {
        let dir = std::env::temp_dir().join(format!("graphrt-channels-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("channels.yaml");
        let vars = [(SLACK_BOT_TOKEN_ENV, "xoxb-1-2-abc"), (SLACK_APP_TOKEN_ENV, "xapp-1-A-2-abc")];
        std::fs::write(&path, "channels:\n  - { kind: slack, notify: [C0OPS], approvers: [U7] }\n").unwrap();
        let channels = load(&[], Some(&path), Defaults::default(), &env(&vars)).unwrap();
        assert_eq!((channels[0].workflow.as_str(), channels[0].notify.as_slice()), ("", &["C0OPS".to_owned()][..]));
        std::fs::write(&path, "channels:\n  - { kind: slack }\n").unwrap();
        let error = load(&[], Some(&path), Defaults::default(), &env(&vars)).unwrap_err().to_string();
        assert!(error.contains("`notify`"), "{error}");
        let wa = [
            (WHATSAPP_TOKEN_ENV, "t"),
            (WHATSAPP_PHONE_NUMBER_ID_ENV, "1"),
            (WHATSAPP_APP_SECRET_ENV, "s"),
            (WHATSAPP_VERIFY_TOKEN_ENV, "v"),
        ];
        std::fs::write(&path, "channels:\n  - { kind: whatsapp, workflow: w, notify: [\"62811\"] }\n").unwrap();
        assert!(load(&[], Some(&path), Defaults::default(), &env(&wa)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
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
        let channels =
            load(&[], Some(&path), Defaults { rate_limit: 10, budget: 50, language: Language::En }, &env(&vars))
                .unwrap();
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

    fn telegram_config(input: Option<&str>) -> ChannelConfig {
        let mut config = load_flags(&["telegram=support"], &[(TELEGRAM_TOKEN_ENV, "123456:token")]).unwrap().remove(0);
        config.input = input.map(str::to_owned);
        config
    }

    #[test]
    fn the_message_goes_to_the_one_input_a_run_needs_and_impossible_setups_are_refused() {
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
        // Nothing required: `sys.query`.
        assert_eq!(message_input(&telegram_config(None), &[], Some(names(&["note"]))).unwrap(), None);
        // One required input takes the message.
        let one = message_input(&telegram_config(None), &names(&["message"]), Some(names(&["message", "note"])));
        assert_eq!(one.unwrap().as_deref(), Some("message"));
        // A named input must exist.
        let missing = message_input(&telegram_config(Some("text")), &[], Some(names(&["message"]))).unwrap_err();
        assert!(missing.to_string().contains("declares no start input `text`"), "{missing}");
        // Two required inputs: the message fills one, the server says which syntax names it.
        let two =
            message_input(&telegram_config(None), &names(&["message", "city"]), Some(names(&["message", "city"])));
        assert!(two.unwrap_err().to_string().contains("telegram=support=INPUT"));
        let other = message_input(
            &telegram_config(Some("message")),
            &names(&["message", "city"]),
            Some(names(&["message", "city"])),
        );
        assert!(other.unwrap_err().to_string().contains("also requires city"));
        // A workflow that is not served is reported elsewhere.
        assert_eq!(message_input(&telegram_config(Some("x")), &[], None).unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn the_servers_sentences_follow_the_language_and_single_ones_can_be_replaced() {
        assert_eq!(Messages::of(Language::Id).error, "Maaf, ada kendala di sisi kami. Silakan coba lagi.");
        assert_eq!(Language::parse(" ID "), Some(Language::Id));
        assert_eq!(Language::parse("fr"), None);
        let overrides =
            MessageOverrides { welcome: Some("Selamat datang di Toko Kue Sari".into()), ..MessageOverrides::default() };
        let messages = overrides.apply(Messages::of(Language::Id));
        assert_eq!(messages.welcome, "Selamat datang di Toko Kue Sari");
        assert_eq!(messages.busy, Messages::of(Language::Id).busy);
    }
}
