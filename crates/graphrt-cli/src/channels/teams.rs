//! The Teams side of the channels (cargo feature `channel-teams`): connect the
//! Azure Bot and take what Teams posts to its messaging endpoint, after its
//! Bot Framework token passed. Everything after a message arrived is the same
//! as for every channel ([`super::Channels`]).

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use graphrt_channel::teams::{self, TeamsBot, TeamsKeys, TokenRules};
use graphrt_channel::{Channel, ChannelError, ChannelKind, ChatKind, InboundEvent};

use super::{Bot, ChannelConfig, Channels, Credentials};
use crate::error::CliError;
use crate::serve::error_response;

/// The bot and the keys that sign what Teams posts to it.
#[derive(Clone, Debug)]
pub(super) struct Teams {
    bot: Arc<TeamsBot>,
    keys: Arc<TeamsKeys>,
    issuers: Vec<String>,
}

/// The bot of a Teams channel.
pub(super) fn connect(config: ChannelConfig, http: &reqwest::Client) -> Result<Bot, CliError> {
    let refused = |error: ChannelError| CliError::usage(format!("channel `{}`: {error}", config.name));
    let Credentials::Teams { app_id, app_password, tenant_id, login_base, openid_configuration } = &config.credentials
    else {
        return Err(CliError::usage(format!("channel `{}`: not a Teams channel", config.name)));
    };
    let mut bot = TeamsBot::new(http.clone(), app_id, app_password.clone(), tenant_id.clone()).map_err(refused)?;
    if let Some(base) = login_base {
        bot = bot.with_login_base(base);
    }
    if let Some(service_url) = &config.api_base {
        bot = bot.with_service_url(service_url);
    }
    let configurations = match openid_configuration {
        Some(url) => vec![url.clone()],
        None => teams::openid_configurations(tenant_id.as_deref()),
    };
    let teams = Teams {
        keys: Arc::new(TeamsKeys::new(http.clone(), configurations)),
        issuers: teams::issuers(tenant_id.as_deref()),
        bot: Arc::new(bot),
    };
    let channel: Arc<dyn Channel> = Arc::clone(&teams.bot) as Arc<dyn Channel>;
    Ok(Bot { teams: Some(teams), ..Bot::new(config, channel) })
}

/// Say where the Azure Bot's messaging endpoint must point.
pub(super) fn start(channels: &Arc<Channels>, bot: &Arc<Bot>) {
    if bot.config.kind != ChannelKind::Teams {
        return;
    }
    let address = channels.public_url.as_deref().unwrap_or("https://<your address>");
    eprintln!(
        "channel `{}`: set the Messaging endpoint of the Azure Bot to {address}{}, and add the Microsoft Teams \
         channel to it",
        bot.name(),
        bot.webhook_path()
    );
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// `POST /v1/channels/{name}/webhook` of a Teams channel: one activity, taken
/// only when its Bot Framework token passes.
pub(super) async fn deliver(channels: &Arc<Channels>, bot: &Arc<Bot>, headers: &HeaderMap, body: &[u8]) -> Response {
    let Some(teams) = &bot.teams else { return error_response(StatusCode::NOT_FOUND, "unknown channel") };
    // Read first, act only after the token passed: its claims are checked against the activity.
    let Ok(activity) = serde_json::from_slice::<serde_json::Value>(body) else {
        return error_response(StatusCode::BAD_REQUEST, "the delivery is not JSON");
    };
    let parsed = teams::parse_activity(&activity);
    let authorization = headers.get(axum::http::header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let rules = TokenRules {
        app_id: teams.bot.app_id(),
        issuers: &teams.issuers,
        service_url: parsed.service_url.as_deref(),
        channel_id: parsed.channel_id.as_deref(),
        now_secs: now_secs(),
    };
    if let Err(refusal) = teams.keys.verify(authorization, &rules).await {
        eprintln!("channel `{}`: refused a delivery: {refusal}", bot.name());
        return error_response(StatusCode::UNAUTHORIZED, "the delivery is not signed by the channel");
    }
    // A single tenant bot takes its own organisation only.
    if let (Some(own), Some(from)) = (teams.bot.tenant(), parsed.tenant_id.as_deref())
        && own != from
    {
        return error_response(StatusCode::FORBIDDEN, "this bot serves another organisation");
    }
    if let Some(consent) = parsed.file_consent {
        // Teams waits for the invoke's answer: the upload happens after it.
        let teams = teams.clone();
        let name = bot.name().to_owned();
        graphrt_core::task::spawn(async move {
            if let Err(error) = teams.bot.answer_file_consent(&consent).await {
                eprintln!("warning: channel `{name}`: a file was not sent: {error}");
            }
        });
        return (StatusCode::OK, axum::Json(serde_json::json!({}))).into_response();
    }
    if let InboundEvent::Message(message) = &parsed.event {
        if let Some(service_url) = &parsed.service_url {
            teams.bot.remember_service_url(&message.chat_id, service_url);
        }
        if message.chat_kind == ChatKind::Private {
            teams.bot.remember_direct_chat(&message.customer.id, &message.chat_id);
        }
    }
    channels.receive(bot, parsed.event);
    StatusCode::OK.into_response()
}
