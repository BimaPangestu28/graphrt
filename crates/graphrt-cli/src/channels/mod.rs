//! Messaging channels of `graphrt serve` (`--channel KIND=WORKFLOW`, cargo
//! features `channel-telegram` and `channel-whatsapp`): a customer writes on
//! Telegram or WhatsApp, the workflow runs, and the answer goes back on the
//! same channel, with one conversation per customer.
//!
//! What the channel itself needs (payloads, signatures, formatting, sending) is
//! the `graphrt-channel` crate. This module is the host around it:
//!
//! - **One conversation per channel and customer**, in the server's
//!   conversation store. Its id is `ch-<channel>-<chat>`, the run's user is
//!   `<channel>:<customer>`.
//! - **One turn at a time.** While a run of a conversation executes, further
//!   messages of that customer wait, and go into the next turn together.
//! - **Answers leave when a run ends**, whoever started or resumed it
//!   ([`Channels::run_ended`], called by the run's driver). So an answer that an
//!   approval in the inbox released, or a run resumed after a restart, still
//!   reaches the customer.
//! - **A paused run asks.** A question to the customer (`--user-resume`) is sent
//!   to them and their next message answers it. Anything else (a tool approval,
//!   a form) waits for the team: the customer is told, and the request is in the
//!   approvals inbox and with the channel's `approvers`.
//! - **Limits.** Each message id is handled once, each customer has a rate
//!   limit, each channel a budget, and senders can be restricted to a list.
//!
//! A delivery is acknowledged before anything runs: nothing here is awaited by
//! the webhook's answer or the polling loop.

pub mod config;
mod reply;
mod routes;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use graphrt_channel::{Button, Channel, ChannelError, ChannelKind, ChatKind, InboundEvent, InboundMessage};
use graphrt_core::Value;
use graphrt_core::store::{RunRecord, RunStatus, now_ms};
use sha2::{Digest, Sha256};

pub use config::{ChannelConfig, Credentials, Defaults, Language, Mode};

/// The start inputs of a served workflow, as a channel needs them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartInputs {
    /// Every start input.
    pub declared: Vec<String>,
    /// Those a run cannot start without (required, no default).
    pub required: Vec<String>,
}
pub(crate) use routes::routes;

use crate::approvals::{Request, requests_of};
use crate::conversations::{ChannelLink, Conversation, Role};
use crate::error::CliError;
use crate::serve::{AppState, ChannelTurn, TurnRefused};
use crate::user_auth::rate_limit::UserRateLimiter;

/// Prefix of the conversation ids of channel conversations.
const CONVERSATION_PREFIX: &str = "ch-";
/// Message ids remembered to drop redeliveries.
const SEEN_CAP: usize = 20_000;
/// Messages of one customer that wait while their run executes.
const MAX_QUEUED: usize = 20;
/// Bytes of one turn's text. Channels cap a message at 4096 characters.
const MAX_TURN_BYTES: usize = 16 * 1024;
/// Files of one turn.
const MAX_TURN_FILES: usize = 8;
/// How long the typing sign is kept up for one run.
const MAX_TYPING: Duration = Duration::from_secs(10 * 60);
/// How long a turn waits for its first typing sign before the run starts anyway.
const FIRST_SIGN_WAIT: Duration = Duration::from_secs(3);
/// The least time between two notices of the same kind to one customer.
const NOTICE_INTERVAL: Duration = Duration::from_secs(60);
/// Conversations remembered at once before the ones at rest are dropped.
const MAX_CHATS: usize = 10_000;
/// How long one long polling call waits.
#[cfg(feature = "channel-telegram")]
const POLL_WAIT_SECS: u64 = 25;

/// Whether `session_id` is the id of a channel conversation.
pub fn is_channel_conversation(session_id: &str) -> bool {
    session_id.starts_with(CONVERSATION_PREFIX)
}

/// The conversation id of `chat_id` on the channel `name`.
fn conversation_id(name: &str, chat_id: &str) -> String {
    let chat: String =
        chat_id.chars().take(80).map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("{CONVERSATION_PREFIX}{name}-{chat}")
}

/// The run id of the message `message_id` on the channel `name`: the same for
/// a redelivery, so the run store refuses to start it twice.
fn run_id(name: &str, message_id: &str) -> String {
    let digest = Sha256::digest(message_id.as_bytes());
    format!("chn-{name}-{}", crate::hex::to_hex(digest.get(..12).unwrap_or_default()))
}

/// One connected channel.
pub(crate) struct Bot {
    config: ChannelConfig,
    channel: Arc<dyn Channel>,
    #[cfg(feature = "channel-telegram")]
    telegram: Option<graphrt_channel::telegram::TelegramBot>,
    #[cfg(feature = "channel-whatsapp")]
    whatsapp: Option<graphrt_channel::whatsapp::WhatsAppCloud>,
    per_customer: Option<UserRateLimiter>,
    budget: Option<UserRateLimiter>,
}

impl std::fmt::Debug for Bot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bot").field("name", &self.config.name).field("kind", &self.config.kind).finish_non_exhaustive()
    }
}

impl Bot {
    fn name(&self) -> &str {
        &self.config.name
    }

    /// The path Telegram or Meta posts to.
    fn webhook_path(&self) -> String {
        format!("/v1/channels/{}/webhook", self.config.name)
    }

    /// Send `markdown`, logging a failure. Returns whether it was sent.
    async fn say(&self, chat_id: &str, markdown: &str, buttons: &[Button]) -> bool {
        if markdown.trim().is_empty() {
            return true;
        }
        match self.channel.send_text(chat_id, markdown, buttons).await {
            Ok(_) => true,
            Err(error) => {
                eprintln!("warning: channel `{}`: a message was not sent: {error}", self.name());
                false
            }
        }
    }
}

/// What the server remembers of one conversation while it runs.
#[derive(Default)]
struct Chat {
    /// A turn is being worked on.
    busy: bool,
    /// Messages that arrived meanwhile.
    queue: Vec<InboundMessage>,
    typing: Option<tokio::task::JoinHandle<()>>,
    /// The numbered choices of the last answer.
    choices: Vec<reply::Choice>,
    /// When the customer was last told to wait or slow down.
    last_notice: Option<Instant>,
}

/// A request an approver may answer in the channel.
#[derive(Clone, Debug)]
struct Pending {
    run_id: String,
    key: String,
    question: reply::Question,
    /// Which run and customer, shown above the question.
    heading: String,
}

#[derive(Default)]
struct Seen {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

/// The messaging channels of a server.
pub struct Channels {
    bots: Vec<Arc<Bot>>,
    /// The address clients reach the server at, for webhook registration.
    public_url: Option<String>,
    /// The customer answers the questions of their own runs (`--user-resume`).
    customer_answers: bool,
    state: OnceLock<Weak<AppState>>,
    chats: Mutex<HashMap<String, Chat>>,
    seen: Mutex<Seen>,
    /// By channel and approver: the requests they may answer, oldest first.
    approvals: Mutex<HashMap<(String, String), VecDeque<Pending>>>,
}

impl std::fmt::Debug for Channels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channels").field("bots", &self.bots).finish_non_exhaustive()
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The HTTP client of all channels. No redirects: an API never needs one, and
/// a request carries a credential.
fn http_client() -> Result<reqwest::Client, CliError> {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("graphrt/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CliError::failed(format!("cannot build the HTTP client of the channels: {e}")))
}

fn connect(config: ChannelConfig, http: &reqwest::Client) -> Result<Bot, CliError> {
    let refused = |error: ChannelError| CliError::usage(format!("channel `{}`: {error}", config.name));
    let limiter = |per_minute: u32| (per_minute > 0).then(|| UserRateLimiter::per_minute(per_minute));
    let (per_customer, budget) = (limiter(config.rate_limit), limiter(config.budget));
    match &config.credentials {
        #[cfg(feature = "channel-telegram")]
        Credentials::Telegram { token, .. } => {
            let mut bot = graphrt_channel::telegram::TelegramBot::new(http.clone(), token.clone()).map_err(refused)?;
            if let Some(base) = &config.api_base {
                bot = bot.with_base_url(base);
            }
            Ok(Bot {
                channel: Arc::new(bot.clone()),
                telegram: Some(bot),
                #[cfg(feature = "channel-whatsapp")]
                whatsapp: None,
                per_customer,
                budget,
                config,
            })
        }
        #[cfg(feature = "channel-whatsapp")]
        Credentials::WhatsApp { access_token, phone_number_id, api_version, .. } => {
            let mut cloud =
                graphrt_channel::whatsapp::WhatsAppCloud::new(http.clone(), phone_number_id, access_token.clone())
                    .map_err(refused)?;
            if let Some(base) = &config.api_base {
                cloud = cloud.with_base_url(base);
            }
            if let Some(version) = api_version {
                cloud = cloud.with_api_version(version).map_err(refused)?;
            }
            Ok(Bot {
                channel: Arc::new(cloud.clone()),
                #[cfg(feature = "channel-telegram")]
                telegram: None,
                whatsapp: Some(cloud),
                per_customer,
                budget,
                config,
            })
        }
        #[allow(unreachable_patterns)]
        _ => Err(CliError::usage(format!(
            "channel `{}`: `{}` is not built into this binary (cargo feature `channel-{}`)",
            config.name, config.kind, config.kind
        ))),
    }
}

impl Channels {
    /// The channels of `configs`. `serves` tells whether a workflow is served,
    /// `public_url` is the address clients reach the server at and
    /// `customer_answers` whether a customer answers the questions of their own
    /// runs (`--user-resume`).
    ///
    /// @returns `None` without any channel.
    /// @throws CliError (usage) - A channel of a workflow that is not served, or unusable credentials.
    pub fn new(
        configs: Vec<ChannelConfig>,
        start_inputs: impl Fn(&str) -> Option<StartInputs>,
        public_url: Option<String>,
        customer_answers: bool,
    ) -> Result<Option<Self>, CliError> {
        if configs.is_empty() {
            return Ok(None);
        }
        let http = http_client()?;
        let mut bots = Vec::with_capacity(configs.len());
        for mut config in configs {
            let Some(inputs) = start_inputs(&config.workflow) else {
                return Err(CliError::usage(format!(
                    "channel `{}` names workflow `{}`, which is not served",
                    config.name, config.workflow
                )));
            };
            config.input = config::message_input(&config, &inputs.required, Some(inputs.declared))?;
            bots.push(Arc::new(connect(config, &http)?));
        }
        Ok(Some(Self {
            bots,
            public_url,
            customer_answers,
            state: OnceLock::new(),
            chats: Mutex::new(HashMap::new()),
            seen: Mutex::new(Seen::default()),
            approvals: Mutex::new(HashMap::new()),
        }))
    }

    /// Lines for the startup banner.
    pub fn describe(&self) -> Vec<String> {
        self.bots
            .iter()
            .map(|bot| {
                let config = &bot.config;
                let how = match (config.kind, config.mode) {
                    (ChannelKind::Telegram, Mode::Polling) => "long polling, no public address needed".to_owned(),
                    _ => format!("webhook at {}", bot.webhook_path()),
                };
                let who = if config.allow.is_empty() {
                    String::new()
                } else {
                    format!(", {} allowed sender(s)", config.allow.len())
                };
                let input =
                    config.input.as_deref().map(|input| format!(", message into `{input}`")).unwrap_or_default();
                format!("channel `{}` ({}) -> {} ({how}{who}{input})", config.name, config.kind, config.workflow)
            })
            .collect()
    }

    fn state(&self) -> Option<Arc<AppState>> {
        self.state.get().and_then(Weak::upgrade)
    }

    fn bot(&self, name: &str) -> Option<&Arc<Bot>> {
        self.bots.iter().find(|bot| bot.name() == name)
    }

    /// Begin receiving: poll Telegram, or register its webhook. Call once the
    /// state is shared. Channels keep the state weakly, so they never keep a
    /// server alive.
    pub fn start(self: &Arc<Self>, state: &Arc<AppState>) {
        if self.state.set(Arc::downgrade(state)).is_err() {
            return;
        }
        for bot in &self.bots {
            #[cfg(feature = "channel-telegram")]
            if let Some(telegram) = bot.telegram.clone() {
                let (channels, bot) = (Arc::clone(self), Arc::clone(bot));
                graphrt_core::task::spawn(async move {
                    match bot.config.mode {
                        Mode::Polling => channels.poll(&bot, &telegram).await,
                        Mode::Webhook => channels.register_webhook(&bot, &telegram).await,
                    }
                });
            }
            #[cfg(feature = "channel-whatsapp")]
            if bot.whatsapp.is_some() {
                let address = self.public_url.as_deref().unwrap_or("https://<your address>");
                eprintln!(
                    "channel `{}`: set the webhook of the WhatsApp app to {address}{} with your verify token, \
                     and subscribe it to `messages`",
                    bot.name(),
                    bot.webhook_path()
                );
            }
        }
    }

    /// Tell Telegram where to post updates, when the server knows its own address.
    #[cfg(feature = "channel-telegram")]
    async fn register_webhook(&self, bot: &Bot, telegram: &graphrt_channel::telegram::TelegramBot) {
        let Credentials::Telegram { webhook_secret: Some(secret), .. } = &bot.config.credentials else { return };
        let Some(public_url) = &self.public_url else {
            eprintln!(
                "channel `{}`: webhook mode, but the server does not know its public address. Start it with \
                 --public-url https://<your address>, or register {} with Telegram yourself (setWebhook)",
                bot.name(),
                bot.webhook_path()
            );
            return;
        };
        let url = format!("{public_url}{}", bot.webhook_path());
        match telegram.set_webhook(&url, secret).await {
            Ok(()) => eprintln!("channel `{}`: Telegram posts updates to {url}", bot.name()),
            Err(error) => eprintln!("warning: channel `{}`: the webhook was not registered: {error}", bot.name()),
        }
    }

    /// Long polling: ask Telegram for updates until the server stops.
    #[cfg(feature = "channel-telegram")]
    async fn poll(self: &Arc<Self>, bot: &Arc<Bot>, telegram: &graphrt_channel::telegram::TelegramBot) {
        // A webhook left from an earlier setup would make every polling call fail.
        match telegram.webhook_url().await {
            Ok(Some(_)) => match telegram.delete_webhook().await {
                Ok(()) => eprintln!("channel `{}`: removed the bot's webhook, this server polls instead", bot.name()),
                Err(error) => eprintln!("warning: channel `{}`: cannot remove the bot's webhook: {error}", bot.name()),
            },
            Ok(None) => {}
            Err(error) => eprintln!("warning: channel `{}`: {error}", bot.name()),
        }
        let mut offset = None;
        let mut pause = Duration::from_secs(1);
        loop {
            let Some(state) = self.state() else { return };
            if state.is_draining() {
                return;
            }
            drop(state);
            match telegram.get_updates(offset, POLL_WAIT_SECS).await {
                Ok(updates) => {
                    pause = Duration::from_secs(1);
                    for update in &updates {
                        let parsed = graphrt_channel::telegram::parse_update(update);
                        if let Some(update_id) = parsed.update_id {
                            offset = Some(update_id.saturating_add(1));
                        }
                        self.receive(bot, parsed.event);
                    }
                }
                Err(ChannelError::Api { status: 401 | 404, .. }) => {
                    eprintln!(
                        "error: channel `{}`: Telegram refused the bot token. The channel is off until the server \
                         restarts with a valid one",
                        bot.name()
                    );
                    return;
                }
                Err(error) => {
                    eprintln!("warning: channel `{}`: {error}. Trying again in {} s", bot.name(), pause.as_secs());
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    /// Whether `id` is seen for the first time on `bot`.
    fn first_time(&self, bot: &Bot, id: &str) -> bool {
        let key = format!("{}\n{id}", bot.name());
        let mut seen = locked(&self.seen);
        if !seen.ids.insert(key.clone()) {
            return false;
        }
        seen.order.push_back(key);
        if seen.order.len() > SEEN_CAP
            && let Some(oldest) = seen.order.pop_front()
        {
            seen.ids.remove(&oldest);
        }
        true
    }

    /// Whether a notice may be sent to this conversation now: at most one per
    /// minute, so a flood of messages is not answered with a flood.
    fn may_notify(&self, conversation: &str) -> bool {
        let mut chats = locked(&self.chats);
        let now = Instant::now();
        if chats.len() >= MAX_CHATS {
            // Senders over their limit leave an entry each: drop the ones at rest.
            chats.retain(|_, chat| {
                chat.busy || chat.last_notice.is_some_and(|last| now.duration_since(last) < NOTICE_INTERVAL)
            });
        }
        let chat = chats.entry(conversation.to_owned()).or_default();
        if chat.last_notice.is_some_and(|last| now.duration_since(last) < NOTICE_INTERVAL) {
            return false;
        }
        chat.last_notice = Some(now);
        true
    }

    fn notify(self: &Arc<Self>, bot: &Arc<Bot>, chat_id: &str, conversation: &str, text: &str) {
        if !self.may_notify(conversation) {
            return;
        }
        let (bot, chat_id, text) = (Arc::clone(bot), chat_id.to_owned(), text.to_owned());
        graphrt_core::task::spawn(async move {
            bot.say(&chat_id, &text, &[]).await;
        });
    }

    /// Take what a channel delivered. Returns at once: the work happens in the background.
    pub(crate) fn receive(self: &Arc<Self>, bot: &Arc<Bot>, event: InboundEvent) {
        match event {
            InboundEvent::Message(message) => self.on_message(bot, *message),
            InboundEvent::Status(status) => {
                if let Some(error) = status.error {
                    eprintln!("warning: channel `{}`: a message was not delivered: {error}", bot.name());
                }
            }
            _ => {}
        }
    }

    fn on_message(self: &Arc<Self>, bot: &Arc<Bot>, message: InboundMessage) {
        let config = &bot.config;
        if !self.first_time(bot, &message.id) {
            return;
        }
        if message.chat_kind == ChatKind::Group && !config.groups {
            return;
        }
        if !config.allow.is_empty() && !config.allow.contains(&message.customer.id) {
            eprintln!("channel `{}`: ignored a message of a sender that is not in `allow`", bot.name());
            return;
        }
        if message.chat_kind == ChatKind::Private
            && let Some((pending, input)) = self.approver_answer(bot, &message)
        {
            let (channels, bot) = (Arc::clone(self), Arc::clone(bot));
            graphrt_core::task::spawn(async move {
                channels.resolve_approval(&bot, &message.chat_id, pending, input).await;
            });
            return;
        }
        let conversation = conversation_id(bot.name(), &message.chat_id);
        let now = Instant::now();
        // The customer's own limit first: a customer over it does not use up the budget.
        if bot.per_customer.as_ref().is_some_and(|limiter| limiter.check(&conversation, now).is_err()) {
            self.notify(bot, &message.chat_id, &conversation, &config.messages.slow_down);
            return;
        }
        if bot.budget.as_ref().is_some_and(|budget| budget.check("", now).is_err()) {
            self.notify(bot, &message.chat_id, &conversation, &config.messages.busy);
            return;
        }
        {
            let mut chats = locked(&self.chats);
            let chat = chats.entry(conversation.clone()).or_default();
            if chat.busy {
                if chat.queue.len() < MAX_QUEUED {
                    chat.queue.push(message);
                }
                return;
            }
            chat.busy = true;
        }
        self.spawn_turn(bot, conversation, vec![message]);
    }

    fn spawn_turn(self: &Arc<Self>, bot: &Arc<Bot>, conversation: String, messages: Vec<InboundMessage>) {
        let (channels, bot) = (Arc::clone(self), Arc::clone(bot));
        graphrt_core::task::spawn(async move {
            let running = channels.turn(&bot, &conversation, messages).await;
            if !running {
                channels.release(&bot, &conversation);
            }
        });
    }

    /// The conversation is free again: stop the typing sign and start the turn
    /// of what arrived meanwhile.
    fn release(self: &Arc<Self>, bot: &Arc<Bot>, conversation: &str) {
        let queued = {
            let mut chats = locked(&self.chats);
            let Some(chat) = chats.get_mut(conversation) else { return };
            if let Some(typing) = chat.typing.take() {
                typing.abort();
            }
            let queued = std::mem::take(&mut chat.queue);
            chat.busy = !queued.is_empty();
            // Nothing left to remember about a conversation at rest.
            let recent = chat.last_notice.is_some_and(|last| last.elapsed() < NOTICE_INTERVAL);
            if !chat.busy && chat.choices.is_empty() && !recent {
                chats.remove(conversation);
            }
            queued
        };
        if !queued.is_empty() {
            self.spawn_turn(bot, conversation.to_owned(), queued);
        }
    }

    /// Show the first typing sign of a turn, before its run starts. Awaited,
    /// not left to [`Channels::keep_typing`]: that task stops when the run
    /// ends, and a quick run would end it before the sign went out. The sign
    /// also answers a Telegram tap (the button spins until then) and marks a
    /// WhatsApp message as read, so it must not be lost, and it must not reach
    /// the customer after the answer. Bounded, so a slow channel only delays it.
    async fn show_typing(bot: &Bot, message: &InboundMessage) {
        // Best effort: the answer does not depend on it.
        let _shown = tokio::time::timeout(FIRST_SIGN_WAIT, bot.channel.typing(message)).await;
    }

    /// Keep the typing sign up while the run works, after [`Channels::show_typing`] showed it first.
    fn keep_typing(&self, bot: &Arc<Bot>, conversation: &str, message: InboundMessage) {
        let every = match bot.config.kind {
            ChannelKind::Telegram => Duration::from_secs(4),
            _ => Duration::from_secs(20),
        };
        let bot = Arc::clone(bot);
        let task = tokio::spawn(async move {
            let started = Instant::now();
            loop {
                tokio::time::sleep(every).await;
                if started.elapsed() >= MAX_TYPING {
                    break;
                }
                // Best effort: the answer does not depend on it.
                let _shown = bot.channel.typing(&message).await;
            }
        });
        let mut chats = locked(&self.chats);
        match chats.get_mut(conversation) {
            Some(chat) => {
                if let Some(previous) = chat.typing.replace(task) {
                    previous.abort();
                }
            }
            None => task.abort(),
        }
    }

    /// One turn: the messages of one customer that arrived together.
    ///
    /// @returns Whether a run now executes for the conversation. Its end
    ///   ([`Channels::run_ended`]) then frees the conversation.
    async fn turn(self: &Arc<Self>, bot: &Arc<Bot>, conversation_id: &str, messages: Vec<InboundMessage>) -> bool {
        let Some(state) = self.state() else { return false };
        let (Some(conversations), Some(first)) = (state.conversations().cloned(), messages.first().cloned()) else {
            return false;
        };
        let config = &bot.config;
        let chat_id = first.chat_id.clone();
        let user_id = format!("{}:{}", config.name, first.customer.id);
        let link = ChannelLink {
            name: config.name.clone(),
            kind: config.kind.as_str().to_owned(),
            chat_id: chat_id.clone(),
            customer_name: first.customer.name.clone(),
            last_inbound_ms: now_ms(),
            human: false,
            waiting_run: None,
        };
        let opened =
            conversations.open_channel(conversation_id, &user_id, &config.workflow, config.input.clone(), link).await;
        if let Err(error) = opened {
            eprintln!("warning: channel `{}`: cannot open a conversation: {error}", bot.name());
            bot.say(&chat_id, &config.messages.error, &[]).await;
            return false;
        }
        let name = first.customer.name.clone();
        let touched = conversations
            .change(conversation_id, move |conversation, _| {
                if let Some(link) = &mut conversation.channel {
                    link.last_inbound_ms = now_ms();
                    if name.is_some() {
                        link.customer_name = name;
                    }
                }
            })
            .await;
        let Ok(Some(conversation)) = touched else { return false };
        let link = conversation.channel.clone().unwrap_or_default();

        let mut text = messages.iter().map(|m| m.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
        if text.len() > MAX_TURN_BYTES {
            let mut cut = MAX_TURN_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        let tapped = messages.iter().rev().find_map(|message| message.choice.clone());

        #[cfg(feature = "channel-telegram")]
        if config.kind == ChannelKind::Telegram
            && let Some((command, _)) = graphrt_channel::telegram::command(&text)
        {
            match command {
                "start" => {
                    bot.say(&chat_id, &config.messages.welcome, &[]).await;
                    return false;
                }
                "new" => {
                    if let Err(error) = conversations.store().delete(conversation_id).await {
                        eprintln!("warning: channel `{}`: cannot delete a conversation: {error}", bot.name());
                    }
                    locked(&self.chats).remove(conversation_id);
                    bot.say(&chat_id, &config.messages.new_conversation, &[]).await;
                    return false;
                }
                _ => {}
            }
        }

        if link.human {
            // A person of the team answers: keep what the customer wrote, start nothing.
            let kept = if text.is_empty() { "[a message without text]".to_owned() } else { text };
            let stored = conversations
                .change(conversation_id, move |conversation, limits| {
                    conversation.add_message(Role::User, &kept, now_ms(), limits);
                })
                .await;
            if let Err(error) = stored {
                eprintln!("warning: channel `{}`: cannot keep a message: {error}", bot.name());
            }
            return false;
        }

        let mut shown = false;
        if let Some(waiting) = &link.waiting_run {
            Self::show_typing(bot, &first).await;
            shown = true;
            match self.answer_question(&state, bot, &conversation, waiting, &text, tapped.as_deref()).await {
                Waiting::Resumed => {
                    self.keep_typing(bot, conversation_id, first);
                    return true;
                }
                Waiting::Told => return false,
                Waiting::Gone => {
                    let cleared = conversations
                        .change(conversation_id, |conversation, _| {
                            if let Some(link) = &mut conversation.channel {
                                link.waiting_run = None;
                            }
                        })
                        .await;
                    if let Err(error) = cleared {
                        eprintln!("warning: channel `{}`: {error}", bot.name());
                    }
                }
            }
        }

        // A number or a tap after an answer with numbered choices means that choice.
        let picked = {
            let chats = locked(&self.chats);
            let choices = chats.get(conversation_id).map(|chat| chat.choices.as_slice()).unwrap_or_default();
            reply::pick(choices, tapped.as_deref().unwrap_or(&text)).map(|choice| choice.title.clone())
        };
        if let Some(title) = picked {
            text = title;
        }

        let attachments: Vec<_> = messages.iter().flat_map(|m| m.attachments.iter().cloned()).collect();
        if text.is_empty() && attachments.is_empty() {
            if messages.iter().any(|message| message.unsupported.is_some()) {
                bot.say(&chat_id, &config.messages.unsupported, &[]).await;
            }
            return false;
        }
        if !shown {
            Self::show_typing(bot, &first).await;
        }
        self.keep_typing(bot, conversation_id, first.clone());
        let files = self.take_files(&state, bot, &chat_id, &user_id, &attachments).await;
        if text.is_empty() && files.is_empty() {
            return false;
        }
        let turn = ChannelTurn {
            run_id: run_id(bot.name(), &first.id),
            conversation,
            message: text,
            user_id,
            files,
            channel: config.kind.as_str().to_owned(),
        };
        match crate::serve::start_channel_turn(&state, turn).await {
            Ok(_) => true,
            // The same message again: its run exists, and answers by itself.
            Err(TurnRefused::Duplicate) => false,
            Err(TurnRefused::Busy) => {
                bot.say(&chat_id, &config.messages.busy, &[]).await;
                false
            }
            Err(TurnRefused::Failed(why)) => {
                eprintln!("warning: channel `{}`: a run did not start: {why}", bot.name());
                bot.say(&chat_id, &config.messages.error, &[]).await;
                false
            }
        }
    }

    /// Fetch what the customer sent into the workspace, as `sys.files` values.
    /// Without a workspace the customer is told files are not taken.
    async fn take_files(
        &self,
        state: &AppState,
        bot: &Bot,
        chat_id: &str,
        user_id: &str,
        attachments: &[graphrt_channel::Attachment],
    ) -> Vec<Value> {
        if attachments.is_empty() {
            return Vec::new();
        }
        let messages = &bot.config.messages;
        #[cfg(feature = "files")]
        if let Some(served) = state.workspace() {
            let view = if served.per_user { served.workspace.scoped(user_id) } else { served.workspace.clone() };
            let mut files = Vec::new();
            for attachment in attachments.iter().take(MAX_TURN_FILES) {
                let download = match bot.channel.download(attachment, bot.config.max_file_bytes).await {
                    Ok(download) => download,
                    Err(ChannelError::TooLarge { .. }) => {
                        bot.say(chat_id, &messages.file_too_large, &[]).await;
                        continue;
                    }
                    Err(error) => {
                        eprintln!("warning: channel `{}`: a file was not fetched: {error}", bot.name());
                        continue;
                    }
                };
                let name = inbox_file_name(download.name.as_deref(), download.mime_type.as_deref());
                // A directory per file: two files of one name never meet, and the name stays the customer's.
                let path = format!("inbox/{}/{:x}/{name}", bot.name(), now_nanos());
                let Ok(path) = graphrt_files::WorkspacePath::parse(&path) else { continue };
                match view.write(&path, download.bytes, false).await {
                    Ok(stored) => files.push(Value::File(Arc::new(view.file_value(&stored)))),
                    Err(error) => {
                        eprintln!("warning: channel `{}`: a file was not kept: {error}", bot.name());
                        bot.say(chat_id, &messages.file_too_large, &[]).await;
                    }
                }
            }
            return files;
        }
        let _ = (state, user_id);
        bot.say(chat_id, &messages.no_files, &[]).await;
        Vec::new()
    }

    /// The customer wrote while the run `run_id` waits.
    async fn answer_question(
        self: &Arc<Self>,
        state: &Arc<AppState>,
        bot: &Arc<Bot>,
        conversation: &Conversation,
        run_id: &str,
        text: &str,
        tapped: Option<&str>,
    ) -> Waiting {
        let record = match state.store().get(run_id).await {
            Ok(Some(record)) if record.status == RunStatus::Paused => record,
            _ => return Waiting::Gone,
        };
        let chat_id = conversation.channel.as_ref().map(|link| link.chat_id.clone()).unwrap_or_default();
        let messages = &bot.config.messages;
        let Some((request, question)) = self.customer_question(bot, &record) else {
            // The team answers this one.
            if self.may_notify(&conversation.id) {
                bot.say(&chat_id, &messages.still_waiting, &[]).await;
            }
            return Waiting::Told;
        };
        let Some(input) = question.answer(text, tapped) else {
            let again = format!("{}\n\n{}", messages.choose, question.text);
            bot.say(&chat_id, &again, &reply::buttons_of(&question.choices)).await;
            return Waiting::Told;
        };
        let inputs = serde_json::Map::from_iter([(request.key.clone(), input)]);
        let answer = crate::serve::resume_detached(state, run_id, inputs).await;
        if answer.status().is_success() {
            return Waiting::Resumed;
        }
        // Answered or expired a moment ago, or the server is busy.
        if answer.status() == axum::http::StatusCode::CONFLICT {
            return Waiting::Gone;
        }
        bot.say(&chat_id, &messages.busy, &[]).await;
        Waiting::Told
    }

    /// The request of a paused run that its customer answers, with its question.
    fn customer_question(&self, bot: &Bot, record: &RunRecord) -> Option<(Request, reply::Question)> {
        if !self.customer_answers {
            return None;
        }
        requests_of(record).into_iter().find_map(|request| {
            // A tool approval is never the customer's to give.
            if request.kind != "human_input" {
                return None;
            }
            let question = reply::question_of(&request, &bot.config.messages);
            question.answerable.then_some((request, question))
        })
    }

    /// An execution ended: send its answer or its question to the customer.
    /// Called by the driver of every run of a channel conversation.
    pub(crate) fn run_ended(self: &Arc<Self>, record: &RunRecord, files: Vec<graphrt_core::value::File>) {
        let Some(conversation) = record.session_id.clone().filter(|id| is_channel_conversation(id)) else { return };
        let (channels, record) = (Arc::clone(self), record.clone());
        graphrt_core::task::spawn(async move {
            channels.deliver(&conversation, &record, files).await;
        });
    }

    async fn deliver(
        self: &Arc<Self>,
        conversation_id: &str,
        record: &RunRecord,
        files: Vec<graphrt_core::value::File>,
    ) {
        let Some(state) = self.state() else { return };
        let Some(conversations) = state.conversations().cloned() else { return };
        let Ok(Some(conversation)) = conversations.store().get(conversation_id).await else { return };
        let Some(link) = conversation.channel.clone() else { return };
        let Some(bot) = self.bot(&link.name).cloned() else { return };
        if record.status == RunStatus::Running {
            return;
        }
        let messages = &bot.config.messages;
        // A pause of a shutdown asks nobody: the next server resumes the run.
        let requests = if record.status == RunStatus::Paused { requests_of(record) } else { Vec::new() };
        // The run's state is kept before anything is sent, so that whoever sees
        // what was sent finds it already: a question finds its run waiting for
        // the answer, and an answer finds the approvers' copies of its request gone.
        if record.status != RunStatus::Paused {
            self.forget_approvals(&record.run_id);
        }
        let waiting_run = (!requests.is_empty()).then(|| record.run_id.clone());
        let changed = conversations
            .change(conversation_id, move |conversation, _| {
                if let Some(link) = &mut conversation.channel {
                    link.waiting_run = waiting_run;
                }
            })
            .await;
        if let Err(error) = changed {
            eprintln!("warning: channel `{}`: cannot update a conversation: {error}", bot.name());
        }
        match record.status {
            RunStatus::Succeeded => self.send_answer(&state, &bot, &conversation, &link, record, files).await,
            RunStatus::Paused => {
                if !requests.is_empty() {
                    self.ask(&bot, &conversation, &link, record, &requests).await;
                }
            }
            _ => {
                let expired = record.error.as_deref() == Some(crate::approvals::EXPIRED_REASON);
                let text = if expired { &messages.expired } else { &messages.error };
                self.say_in_window(&bot, &link, text, &[]).await;
            }
        }
        self.release(&bot, conversation_id);
    }

    /// Send free text to the customer of `link`, unless the channel no longer
    /// takes any (WhatsApp, 24 hours after the customer's last message): then
    /// the configured template goes out instead, or nothing.
    async fn say_in_window(&self, bot: &Bot, link: &ChannelLink, markdown: &str, buttons: &[Button]) -> bool {
        #[cfg(feature = "channel-whatsapp")]
        if let Some(cloud) = &bot.whatsapp
            && !graphrt_channel::whatsapp::service_window_open(link.last_inbound_ms, now_ms())
        {
            let Credentials::WhatsApp { reengage_template: Some(template), .. } = &bot.config.credentials else {
                eprintln!(
                    "channel `{}`: an answer was not sent, the customer's last message is older than 24 hours. \
                     It is kept in the conversation. Set `reengage_template` to tell the customer to write again",
                    bot.name()
                );
                return false;
            };
            return match cloud.send_template(&link.chat_id, &template.name, &template.language, &[]).await {
                Ok(_) => false,
                Err(error) => {
                    eprintln!("warning: channel `{}`: the template was not sent: {error}", bot.name());
                    false
                }
            };
        }
        bot.say(&link.chat_id, markdown, buttons).await
    }

    async fn send_answer(
        &self,
        state: &AppState,
        bot: &Bot,
        conversation: &Conversation,
        link: &ChannelLink,
        record: &RunRecord,
        files: Vec<graphrt_core::value::File>,
    ) {
        let answer = reply::answer_of(&bot.config.messages, &record.answer, &record.outputs);
        let buttons = reply::buttons_of(&answer.choices);
        // Before the answer goes out, so that a quick pick of one of them finds them.
        if let Some(chat) = locked(&self.chats).get_mut(&conversation.id) {
            chat.choices = answer.choices;
        }
        let sent = self.say_in_window(bot, link, &answer.text, &buttons).await;
        if !sent && !answer.text.trim().is_empty() {
            return;
        }
        let mut produced = files;
        for output in record.outputs.values() {
            collect_files(output, 0, &mut produced);
        }
        for file in produced.iter().take(MAX_TURN_FILES) {
            match self.read_file(state, bot, file).await {
                Some(outbound) => {
                    if let Err(error) = bot.channel.send_file(&link.chat_id, &outbound).await {
                        eprintln!("warning: channel `{}`: a file was not sent: {error}", bot.name());
                    }
                }
                None => {
                    if let Some(url) = file.url.as_deref().filter(|url| url.starts_with("https://")) {
                        bot.say(&link.chat_id, &format!("{}: {url}", file.name), &[]).await;
                    }
                }
            }
        }
    }

    /// The bytes of a file a run produced in the workspace.
    async fn read_file(
        &self,
        state: &AppState,
        bot: &Bot,
        file: &graphrt_core::value::File,
    ) -> Option<graphrt_channel::OutboundFile> {
        #[cfg(feature = "files")]
        {
            let reference = file.reference.as_deref()?;
            let (view, path) = state.workspace()?.workspace.locate_reference(reference)?;
            match view.read_all(&path, bot.config.max_file_bytes).await {
                Ok(bytes) => Some(graphrt_channel::OutboundFile {
                    name: file.name.clone(),
                    mime_type: file.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
                    bytes,
                    caption: None,
                }),
                Err(error) => {
                    eprintln!("warning: channel `{}`: file `{}` was not sent: {error}", bot.name(), file.name);
                    None
                }
            }
        }
        #[cfg(not(feature = "files"))]
        {
            let _ = (state, bot, file);
            None
        }
    }

    /// A run paused for a person: ask the customer, or tell them the team is
    /// asked and hand the request to the approvers.
    async fn ask(
        self: &Arc<Self>,
        bot: &Arc<Bot>,
        conversation: &Conversation,
        link: &ChannelLink,
        record: &RunRecord,
        requests: &[Request],
    ) {
        if let Some((_, question)) = self.customer_question(bot, record) {
            self.say_in_window(bot, link, &question.text, &reply::buttons_of(&question.choices)).await;
            return;
        }
        if self.may_notify(&conversation.id) {
            self.say_in_window(bot, link, &bot.config.messages.waiting, &[]).await;
        }
        let customer = link.customer_name.as_deref().unwrap_or(&link.chat_id);
        let heading = format!("**Approval needed** ({}, customer {customer})", record.workflow);
        let mut answerable = false;
        for request in requests {
            let question = reply::question_of(request, &bot.config.messages);
            if !question.answerable {
                continue;
            }
            answerable = true;
            let pending =
                Pending { run_id: record.run_id.clone(), key: request.key.clone(), question, heading: heading.clone() };
            for approver in &bot.config.approvers {
                let first = {
                    let mut approvals = locked(&self.approvals);
                    let queue = approvals.entry((bot.name().to_owned(), approver.clone())).or_default();
                    queue.push_back(pending.clone());
                    queue.len() == 1
                };
                if first {
                    show_pending(bot, approver, &pending).await;
                }
            }
        }
        if bot.config.approvers.is_empty() || !answerable {
            eprintln!(
                "channel `{}`: run {} waits for a person. Answer it in the approvals inbox (GET /v1/approvals, \
                 or the monitoring page)",
                bot.name(),
                record.run_id
            );
        }
    }

    /// What an approver's message answers, when they have a request waiting
    /// and the message picks one of its choices.
    fn approver_answer(&self, bot: &Bot, message: &InboundMessage) -> Option<(Pending, serde_json::Value)> {
        if !bot.config.approvers.contains(&message.customer.id) {
            return None;
        }
        let approvals = locked(&self.approvals);
        let pending = approvals.get(&(bot.name().to_owned(), message.customer.id.clone()))?.front()?;
        let input = pending.question.answer(&message.text, message.choice.as_deref())?;
        Some((pending.clone(), input))
    }

    /// Drop the requests of `run_id` from every approver's list.
    fn forget_approvals(&self, run_id: &str) {
        let mut approvals = locked(&self.approvals);
        for queue in approvals.values_mut() {
            queue.retain(|pending| pending.run_id != run_id);
        }
        approvals.retain(|_, queue| !queue.is_empty());
    }

    async fn resolve_approval(&self, bot: &Arc<Bot>, approver_chat: &str, pending: Pending, input: serde_json::Value) {
        let Some(state) = self.state() else { return };
        let inputs = serde_json::Map::from_iter([(pending.key.clone(), input)]);
        let answer = crate::serve::resume_detached(&state, &pending.run_id, inputs).await;
        // Everyone's copy of this request is done, whoever answered.
        let next: Vec<(String, Pending)> = {
            let mut approvals = locked(&self.approvals);
            let mut next = Vec::new();
            for ((channel, approver), queue) in approvals.iter_mut() {
                let was_first = queue.front().is_some_and(|first| first.run_id == pending.run_id);
                queue.retain(|other| other.run_id != pending.run_id || other.key != pending.key);
                if channel == bot.name()
                    && was_first
                    && let Some(following) = queue.front()
                {
                    next.push((approver.clone(), following.clone()));
                }
            }
            approvals.retain(|_, queue| !queue.is_empty());
            next
        };
        let said = if answer.status().is_success() {
            "Done. The run continues."
        } else {
            "This request was already answered, or it is closed."
        };
        bot.say(approver_chat, said, &[]).await;
        for (approver, following) in next {
            show_pending(bot, &approver, &following).await;
        }
    }

    /// A message of a person of the team to the customer of `chat_id`, kept in
    /// the conversation as an answer.
    ///
    /// @throws String - Why it was not sent.
    pub(crate) async fn operator_message(&self, bot: &Arc<Bot>, chat_id: &str, text: &str) -> Result<(), String> {
        let state = self.state().ok_or("the server is stopping")?;
        let conversation_id = conversation_id(bot.name(), chat_id);
        let conversation = match state.conversations() {
            Some(conversations) => conversations.store().get(&conversation_id).await.ok().flatten(),
            None => None,
        };
        let Some(link) = conversation.and_then(|conversation| conversation.channel) else {
            return Err("this customer has not written yet: a channel only answers who wrote first".to_owned());
        };
        #[cfg(feature = "channel-whatsapp")]
        if bot.whatsapp.is_some() && !graphrt_channel::whatsapp::service_window_open(link.last_inbound_ms, now_ms()) {
            return Err(ChannelError::OutsideServiceWindow.to_string());
        }
        bot.channel.send_text(&link.chat_id, text, &[]).await.map_err(|error| error.to_string())?;
        if let Some(conversations) = state.conversations() {
            let text = text.to_owned();
            let stored = conversations
                .change(&conversation_id, move |conversation, limits| {
                    conversation.add_message(Role::Assistant, &text, now_ms(), limits);
                })
                .await;
            if let Err(error) = stored {
                eprintln!("warning: channel `{}`: cannot keep a message: {error}", bot.name());
            }
        }
        Ok(())
    }

    /// Hand the customer of `chat_id` to a person of the team (`human`), or back
    /// to the workflow.
    ///
    /// @throws String - When the customer has no conversation.
    pub(crate) async fn set_human(&self, bot: &Bot, chat_id: &str, human: bool) -> Result<(), String> {
        let state = self.state().ok_or("the server is stopping")?;
        let conversations = state.conversations().ok_or("the server keeps no conversations")?;
        let changed = conversations
            .change(&conversation_id(bot.name(), chat_id), move |conversation, _| {
                if let Some(link) = &mut conversation.channel {
                    link.human = human;
                }
            })
            .await
            .map_err(|error| error.to_string())?;
        changed.map(|_| ()).ok_or_else(|| "this customer has not written yet".to_owned())
    }
}

/// What became of a message that arrived while a run waits.
enum Waiting {
    /// It answered the question: the run continues.
    Resumed,
    /// The customer was told something, and the run still waits.
    Told,
    /// The run no longer waits: the message is an ordinary one.
    Gone,
}

async fn show_pending(bot: &Bot, approver: &str, pending: &Pending) {
    let text = format!("{}\n\n{}", pending.heading, pending.question.text);
    bot.say(approver, &text, &reply::buttons_of(&pending.question.choices)).await;
}

#[cfg_attr(not(feature = "files"), allow(dead_code))]
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

/// A workspace file name for what a customer sent: their name without anything
/// but letters, digits, `.`, `-` and `_`, else `file` with the type's extension.
#[cfg_attr(not(feature = "files"), allow(dead_code))]
fn inbox_file_name(name: Option<&str>, mime_type: Option<&str>) -> String {
    let extension = match mime_type.unwrap_or_default() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "video/mp4" => "mp4",
        "text/plain" => "txt",
        _ => "bin",
    };
    let last = name.unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(80)
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    if cleaned.is_empty() {
        format!("file.{extension}")
    } else if cleaned.contains('.') {
        cleaned.to_owned()
    } else {
        format!("{cleaned}.{extension}")
    }
}

/// File values in `value`, at most three levels down.
fn collect_files(value: &Value, depth: usize, files: &mut Vec<graphrt_core::value::File>) {
    match value {
        Value::File(file) => {
            if !files.iter().any(|known| known.reference == file.reference && known.url == file.url) {
                files.push((**file).clone());
            }
        }
        Value::Array(items) if depth < 3 => items.iter().for_each(|item| collect_files(item, depth + 1, files)),
        Value::Object(fields) if depth < 3 => fields.values().for_each(|field| collect_files(field, depth + 1, files)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_safe_as_file_names() {
        assert_eq!(conversation_id("telegram", "42"), "ch-telegram-42");
        assert_eq!(conversation_id("telegram", "-100123"), "ch-telegram--100123");
        assert_eq!(conversation_id("wa", "../../x y"), "ch-wa-______x_y");
        assert!(is_channel_conversation("ch-telegram-42"));
        assert!(!is_channel_conversation("conv-1a-1"));
        let id = run_id("telegram", "42:5");
        assert_eq!(id, run_id("telegram", "42:5"), "a redelivery has the same run id");
        assert_ne!(id, run_id("telegram", "42:6"));
        assert!(id.starts_with("chn-telegram-") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn inbox_names_keep_no_path_and_always_have_an_extension() {
        assert_eq!(inbox_file_name(Some("../../etc/passwd"), None), "passwd.bin");
        assert_eq!(inbox_file_name(Some("Faktur Juli.pdf"), Some("application/pdf")), "Faktur_Juli.pdf");
        assert_eq!(inbox_file_name(None, Some("image/jpeg")), "file.jpg");
        assert_eq!(inbox_file_name(Some("..."), Some("image/png")), "file.png");
    }
}
