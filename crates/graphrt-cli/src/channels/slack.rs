//! The Slack side of the channels (cargo feature `channel-slack`): connect the
//! app, receive by Socket Mode or the Events API, and nothing else. Everything
//! after a message arrived is the same as for every channel ([`super::Channels`]).

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use graphrt_channel::slack::{self, Delivery, SlackBot, SocketEvent, SocketMode};
use graphrt_channel::{Channel, ChannelError};

use super::{Bot, ChannelConfig, Channels, Credentials, Mode};
use crate::error::CliError;
use crate::serve::error_response;

/// The bot of a Slack channel.
pub(super) fn connect(config: ChannelConfig, http: &reqwest::Client) -> Result<Bot, CliError> {
    let refused = |error: ChannelError| CliError::usage(format!("channel `{}`: {error}", config.name));
    let Credentials::Slack { bot_token, app_token, .. } = &config.credentials else {
        return Err(CliError::usage(format!("channel `{}`: not a Slack channel", config.name)));
    };
    let mut bot = SlackBot::new(http.clone(), bot_token.clone()).map_err(refused)?;
    let mut socket =
        app_token.clone().map(|token| SocketMode::new(http.clone(), token)).transpose().map_err(refused)?;
    if let Some(base) = &config.api_base {
        bot = bot.with_base_url(base);
        socket = socket.map(|socket| socket.with_base_url(base));
    }
    let socket = socket.filter(|_| config.mode == Mode::Socket);
    let channel: Arc<dyn Channel> = Arc::new(bot);
    Ok(Bot { slack_socket: socket, ..Bot::new(config, channel) })
}

/// Begin receiving: open the socket, or say where the Events API posts.
pub(super) fn start(channels: &Arc<Channels>, bot: &Arc<Bot>) {
    if bot.config.kind != graphrt_channel::ChannelKind::Slack {
        return;
    }
    match bot.slack_socket.clone() {
        Some(socket) => {
            let (channels, bot) = (Arc::clone(channels), Arc::clone(bot));
            graphrt_core::task::spawn(async move { run_socket(&channels, &bot, &socket).await });
        }
        None => {
            let address = channels.public_url.as_deref().unwrap_or("https://<your address>");
            eprintln!(
                "channel `{}`: set the Request URL of Event Subscriptions and of Interactivity of the Slack app \
                 to {address}{}, and subscribe the bot to `app_mention` and `message.im`",
                bot.name(),
                bot.webhook_path()
            );
        }
    }
}

/// Socket Mode: stay connected until the server stops, opening a new socket
/// when Slack asks for one or the old one breaks.
async fn run_socket(channels: &Arc<Channels>, bot: &Arc<Bot>, socket: &SocketMode) {
    let mut pause = Duration::from_secs(1);
    loop {
        match channels.state() {
            Some(state) if !state.is_draining() => {}
            _ => return,
        }
        let mut connection = match socket.connect().await {
            Ok(connection) => {
                pause = Duration::from_secs(1);
                connection
            }
            Err(ChannelError::Api { description, .. })
                if matches!(description.as_str(), "invalid_auth" | "not_authed" | "token_revoked") =>
            {
                eprintln!(
                    "error: channel `{}`: Slack refused the app token ({description}). The channel is off until the \
                     server restarts with a valid one",
                    bot.name()
                );
                return;
            }
            Err(error) => {
                eprintln!("warning: channel `{}`: {error}. Trying again in {} s", bot.name(), pause.as_secs());
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        loop {
            match connection.next().await {
                Ok(Some(SocketEvent::Event(event))) => channels.receive(bot, event),
                Ok(Some(SocketEvent::Reconnect)) | Ok(None) => break,
                Err(error) => {
                    eprintln!("warning: channel `{}`: {error}, connecting again", bot.name());
                    break;
                }
            }
            if channels.state().is_none_or(|state| state.is_draining()) {
                connection.close().await;
                return;
            }
        }
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// `POST /v1/channels/{name}/webhook` of a Slack channel: an Events API
/// delivery or a button tap, checked by its signature first.
pub(super) fn deliver(channels: &Arc<Channels>, bot: &Arc<Bot>, headers: &HeaderMap, body: &[u8]) -> Response {
    let Credentials::Slack { signing_secret: Some(secret), .. } = &bot.config.credentials else {
        return error_response(StatusCode::NOT_FOUND, "this channel uses Socket Mode: it takes no deliveries");
    };
    let (timestamp, signature) = (header(headers, slack::TIMESTAMP_HEADER), header(headers, slack::SIGNATURE_HEADER));
    if slack::verify_signature(body, timestamp, signature, secret, now_secs()).is_err() {
        return error_response(StatusCode::UNAUTHORIZED, "the delivery is not signed by the channel");
    }
    match slack::parse_delivery(body) {
        Ok(Delivery::Challenge(challenge)) => challenge.into_response(),
        Ok(Delivery::Event(event)) => {
            channels.receive(bot, event);
            StatusCode::OK.into_response()
        }
        Err(_) => error_response(StatusCode::BAD_REQUEST, "the delivery is not a Slack event"),
    }
}
