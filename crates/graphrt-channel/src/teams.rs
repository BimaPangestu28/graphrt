//! Microsoft Teams, through the [Bot Framework](https://learn.microsoft.com/azure/bot-service/rest-api/bot-framework-rest-connector-api-reference)
//! of an Azure Bot.
//!
//! Teams posts every activity to the bot's messaging endpoint with a bearer
//! token of the Bot Connector. [`verify_token`] checks it on the way in: an
//! RS256 signature by one of the connector's published keys
//! ([`TeamsKeys`], feature `client`), the issuer, the bot's app id as
//! audience, the times, the `serviceurl` claim against the activity and the
//! key's endorsement of the channel. Only then is the activity read with
//! [`parse_activity`].
//!
//! | In Teams | [`InboundMessage`] |
//! | --- | --- |
//! | a 1:1 chat with the bot | `chat_id` is the conversation, [`ChatKind::Private`] |
//! | a mention of the bot in a channel post or its replies | `chat_id` is the thread's conversation (`19:...;messageid=...`), [`ChatKind::Group`] |
//! | a group chat | its conversation, [`ChatKind::Group`] |
//! | a tap on an Adaptive Card button the bot sent | `choice` is the button's id |
//!
//! Only activities of the `msteams` channel are read: the same bot can get Web
//! Chat and Direct Line deliveries, whose sender picks its own `from.id`.
//! A person's id is their Microsoft Entra object id (`aadObjectId`), the id an
//! admin sees in the Entra admin center, else the Bot Framework id (`29:...`).
//! The answer goes to the activity's `serviceUrl` (feature `client`, [`TeamsBot`]).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value as Json, json};

use crate::model::{Attachment, AttachmentKind, Button, ChatKind, Customer, InboundEvent, InboundMessage};

/// The issuer of tokens the Bot Connector sends.
pub const CONNECTOR_ISSUER: &str = "https://api.botframework.com";
/// Where the Bot Connector publishes its OpenID configuration (with its keys).
pub const CONNECTOR_OPENID: &str = "https://login.botframework.com/v1/.well-known/openidconfiguration";
/// The key in an Adaptive Card action's `data` that carries the button's id.
pub const CHOICE_KEY: &str = "graphrt_choice";
/// Clock skew allowed for a token's times.
const LEEWAY_SECS: u64 = 5 * 60;
/// The Adaptive Card version Teams draws everywhere.
const CARD_VERSION: &str = "1.5";

/// The issuers a bot of `tenant` accepts: the connector, and for a single
/// tenant bot that tenant's Entra issuers.
pub fn issuers(tenant: Option<&str>) -> Vec<String> {
    let mut issuers = vec![CONNECTOR_ISSUER.to_owned()];
    if let Some(tenant) = tenant.filter(|tenant| !tenant.is_empty()) {
        issuers.push(format!("https://sts.windows.net/{tenant}/"));
        issuers.push(format!("https://login.microsoftonline.com/{tenant}/v2.0"));
    }
    issuers
}

/// The OpenID configurations whose keys sign the tokens a bot of `tenant` gets.
pub fn openid_configurations(tenant: Option<&str>) -> Vec<String> {
    let mut urls = vec![CONNECTOR_OPENID.to_owned()];
    if let Some(tenant) = tenant.filter(|tenant| !tenant.is_empty()) {
        urls.push(format!("https://login.microsoftonline.com/{tenant}/v2.0/.well-known/openid-configuration"));
    }
    urls
}

/// One signing key of the connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningKey {
    pub key_id: String,
    /// RSA modulus and exponent, big endian.
    modulus: Vec<u8>,
    exponent: Vec<u8>,
    /// The channels the key is for (`msteams`...). Empty: any.
    pub endorsements: Vec<String>,
}

/// The RSA keys of a JWKS document (`{"keys": [...]}`). Keys that are not RSA
/// signing keys are left out.
pub fn parse_keys(jwks: &Json) -> Vec<SigningKey> {
    let Some(keys) = jwks.get("keys").and_then(Json::as_array) else { return Vec::new() };
    keys.iter()
        .filter_map(|key| {
            if key.get("kty").and_then(Json::as_str) != Some("RSA") {
                return None;
            }
            if key.get("use").and_then(Json::as_str).is_some_and(|usage| usage != "sig") {
                return None;
            }
            let decode =
                |field: &str| key.get(field).and_then(Json::as_str).and_then(|v| URL_SAFE_NO_PAD.decode(v).ok());
            let endorsements = key
                .get("endorsements")
                .and_then(Json::as_array)
                .map(|list| list.iter().filter_map(Json::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            Some(SigningKey {
                key_id: key.get("kid").and_then(Json::as_str)?.to_owned(),
                modulus: decode("n")?,
                exponent: decode("e")?,
                endorsements,
            })
        })
        .collect()
}

/// What a token must say besides its signature.
#[derive(Clone, Debug)]
pub struct TokenRules<'a> {
    /// The bot's Microsoft app id: the token's audience.
    pub app_id: &'a str,
    /// The accepted issuers ([`issuers`]).
    pub issuers: &'a [String],
    /// The activity's `serviceUrl`, which the token's `serviceurl` claim must name.
    pub service_url: Option<&'a str>,
    /// The activity's `channelId`, which the key must endorse.
    pub channel_id: Option<&'a str>,
    pub now_secs: u64,
}

/// Why a token was refused. Safe to log: it carries no part of the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenRefusal {
    #[error("no bearer token")]
    Missing,
    #[error("not a JWT")]
    Malformed,
    #[error("not signed with RS256")]
    Algorithm,
    /// A key id the published keys do not have: they may have rotated.
    #[error("signed with an unknown key")]
    UnknownKey,
    #[error("the signature does not match")]
    Signature,
    #[error("expired or not valid yet")]
    Time,
    #[error("issued by someone else")]
    Issuer,
    #[error("issued for another bot")]
    Audience,
    #[error("issued for another service address")]
    ServiceUrl,
    #[error("the key is not for this channel")]
    Endorsement,
}

fn segment(text: &str) -> Result<Json, TokenRefusal> {
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| TokenRefusal::Malformed)?;
    let value: Json = serde_json::from_slice(&bytes).map_err(|_| TokenRefusal::Malformed)?;
    if value.is_object() { Ok(value) } else { Err(TokenRefusal::Malformed) }
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Check the `Authorization` header of a delivery against `keys` and `rules`.
///
/// @throws TokenRefusal - The first check that failed.
pub fn verify_token(
    authorization: Option<&str>,
    keys: &[SigningKey],
    rules: &TokenRules<'_>,
) -> Result<(), TokenRefusal> {
    let token = authorization
        .and_then(|value| value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(TokenRefusal::Missing)?;
    let (signed, signature) = token.rsplit_once('.').ok_or(TokenRefusal::Malformed)?;
    let (header, claims) = signed.split_once('.').ok_or(TokenRefusal::Malformed)?;
    let (header, claims) = (segment(header)?, segment(claims)?);
    if header.get("alg").and_then(Json::as_str) != Some("RS256") {
        return Err(TokenRefusal::Algorithm);
    }
    let key_id = header.get("kid").and_then(Json::as_str).ok_or(TokenRefusal::UnknownKey)?;
    let key = keys.iter().find(|key| key.key_id == key_id).ok_or(TokenRefusal::UnknownKey)?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| TokenRefusal::Malformed)?;
    let public = ring::signature::RsaPublicKeyComponents { n: &key.modulus, e: &key.exponent };
    public
        .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .map_err(|_| TokenRefusal::Signature)?;
    let time = |name: &str| claims.get(name).and_then(Json::as_u64);
    let expires = time("exp").ok_or(TokenRefusal::Time)?;
    if rules.now_secs >= expires.saturating_add(LEEWAY_SECS) {
        return Err(TokenRefusal::Time);
    }
    if time("nbf").is_some_and(|nbf| rules.now_secs.saturating_add(LEEWAY_SECS) < nbf) {
        return Err(TokenRefusal::Time);
    }
    let issuer = claims.get("iss").and_then(Json::as_str).unwrap_or_default();
    if !rules.issuers.iter().any(|accepted| accepted == issuer) {
        return Err(TokenRefusal::Issuer);
    }
    let audience_matches = match claims.get("aud") {
        Some(Json::String(audience)) => audience == rules.app_id,
        Some(Json::Array(audiences)) => audiences.iter().any(|a| a.as_str() == Some(rules.app_id)),
        _ => false,
    };
    if !audience_matches || rules.app_id.is_empty() {
        return Err(TokenRefusal::Audience);
    }
    if let Some(claimed) = claims.get("serviceurl").and_then(Json::as_str)
        && !rules.service_url.is_some_and(|service_url| same_url(claimed, service_url))
    {
        return Err(TokenRefusal::ServiceUrl);
    }
    if let Some(channel) = rules.channel_id
        && !key.endorsements.is_empty()
        && !key.endorsements.iter().any(|endorsed| endorsed == channel)
    {
        return Err(TokenRefusal::Endorsement);
    }
    Ok(())
}

/// One activity, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedActivity {
    pub event: InboundEvent,
    /// Where answers to this conversation go. Trust it only after [`verify_token`].
    pub service_url: Option<String>,
    /// `msteams` for Teams.
    pub channel_id: Option<String>,
    /// The tenant (organisation) the activity came from.
    pub tenant_id: Option<String>,
    /// A person's answer to a file consent card (an `invoke` activity).
    /// Answer the delivery with `200`, then hand it to `TeamsBot::answer_file_consent`.
    pub file_consent: Option<FileConsent>,
}

/// What a person answered on a file consent card the bot sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileConsent {
    pub conversation_id: String,
    /// Who answered (their Entra object id, else their Bot Framework id).
    pub from: String,
    pub accepted: bool,
    /// The id the bot put in the card's context.
    pub upload_id: String,
    /// The consent card's activity, to replace once answered.
    pub card_id: Option<String>,
    /// Where to put the bytes (a OneDrive upload session), when accepted.
    pub upload_url: Option<String>,
    /// Where the file lives once uploaded, and its OneDrive ids.
    pub content_url: Option<String>,
    pub name: Option<String>,
    pub unique_id: Option<String>,
    pub file_type: Option<String>,
}

/// The key in a file consent card's context that carries the bot's upload id.
pub const UPLOAD_KEY: &str = "graphrt_upload";

fn file_consent_of(activity: &Json) -> Option<FileConsent> {
    if text_of(activity, "type") != Some("invoke") || text_of(activity, "name") != Some("fileConsent/invoke") {
        return None;
    }
    let value = activity.get("value")?;
    let from = activity.get("from")?;
    let info = value.get("uploadInfo").unwrap_or(&Json::Null);
    let owned = |value: Option<&str>| value.map(str::to_owned);
    Some(FileConsent {
        conversation_id: text_of(activity.get("conversation")?, "id")?.to_owned(),
        from: text_of(from, "aadObjectId").or_else(|| text_of(from, "id"))?.to_owned(),
        accepted: text_of(value, "action") == Some("accept"),
        upload_id: value.get("context").and_then(|context| text_of(context, UPLOAD_KEY))?.to_owned(),
        card_id: owned(text_of(activity, "replyToId")),
        upload_url: owned(text_of(info, "uploadUrl")),
        content_url: owned(text_of(info, "contentUrl")),
        name: owned(text_of(info, "name")),
        unique_id: owned(text_of(info, "uniqueId")),
        file_type: owned(text_of(info, "fileType")),
    })
}

fn text_of<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).filter(|text| !text.is_empty())
}

/// The text a person wrote: mentions (`<at>Bot</at>`) of the bot removed,
/// other markup Teams adds stripped, entities decoded.
pub fn plain_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<at>") {
        out.push_str(rest.get(..open).unwrap_or_default());
        rest = match rest.get(open..).and_then(|tail| tail.find("</at>")) {
            Some(close) => rest.get(open + close + 5..).unwrap_or_default(),
            None => "",
        };
    }
    out.push_str(rest);
    let mut plain = String::with_capacity(out.len());
    let mut in_tag = false;
    for character in out.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => plain.push(character),
            _ => {}
        }
    }
    plain
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn attachments_of(activity: &Json) -> Vec<Attachment> {
    let Some(list) = activity.get("attachments").and_then(Json::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|attachment| {
            let content_type = text_of(attachment, "contentType")?;
            if content_type == "application/vnd.microsoft.teams.file.download.info" {
                let content = attachment.get("content")?;
                let url = text_of(content, "downloadUrl")?;
                return Some(Attachment {
                    file_id: url.to_owned(),
                    kind: AttachmentKind::Document,
                    name: text_of(attachment, "name").map(str::to_owned),
                    mime_type: None,
                    size: None,
                });
            }
            if content_type.starts_with("image/") {
                return Some(Attachment {
                    file_id: text_of(attachment, "contentUrl")?.to_owned(),
                    kind: AttachmentKind::Image,
                    name: text_of(attachment, "name").map(str::to_owned),
                    mime_type: Some(content_type.to_owned()),
                    size: None,
                });
            }
            // The HTML copy of the message text, cards, and anything else.
            None
        })
        .collect()
}

/// Read one activity (the body of a delivery).
pub fn parse_activity(activity: &Json) -> ParsedActivity {
    let service_url = text_of(activity, "serviceUrl").map(str::to_owned);
    let channel_id = text_of(activity, "channelId").map(str::to_owned);
    let conversation = activity.get("conversation").unwrap_or(&Json::Null);
    let tenant_id = conversation
        .get("tenantId")
        .and_then(Json::as_str)
        .or_else(|| activity.get("channelData").and_then(|data| data.get("tenant")).and_then(|t| text_of(t, "id")))
        .map(str::to_owned);
    let parsed = |event| ParsedActivity {
        event,
        service_url: service_url.clone(),
        channel_id: channel_id.clone(),
        tenant_id: tenant_id.clone(),
        file_consent: None,
    };
    if let Some(consent) = file_consent_of(activity) {
        return ParsedActivity {
            file_consent: Some(consent),
            ..parsed(InboundEvent::Ignored("a file consent answer"))
        };
    }
    if text_of(activity, "type") != Some("message") {
        return parsed(InboundEvent::Ignored("not a message"));
    }
    // The connector also signs Web Chat and Direct Line deliveries of the same
    // bot, where the sender writes its own `from.id`: a person's id is only
    // known from Teams.
    if channel_id.as_deref() != Some("msteams") {
        return parsed(InboundEvent::Ignored("not from Microsoft Teams"));
    }
    let from = activity.get("from").unwrap_or(&Json::Null);
    if text_of(from, "role") == Some("bot") {
        return parsed(InboundEvent::Ignored("written by a bot"));
    }
    let (Some(conversation_id), Some(activity_id), Some(sender)) = (
        text_of(conversation, "id"),
        text_of(activity, "id"),
        text_of(from, "aadObjectId").or_else(|| text_of(from, "id")),
    ) else {
        return parsed(InboundEvent::Ignored("a message without ids"));
    };
    let chat_kind = match text_of(conversation, "conversationType") {
        Some("personal") | None => ChatKind::Private,
        Some(_) => ChatKind::Group,
    };
    let value = activity.get("value");
    let choice = value.and_then(|value| match value.get(CHOICE_KEY) {
        Some(Json::String(id)) => Some(id.clone()),
        Some(Json::Number(id)) => Some(id.to_string()),
        _ => None,
    });
    let title = value.and_then(|value| text_of(value, "graphrt_title")).map(str::to_owned);
    let text = match (&choice, title) {
        (Some(_), Some(title)) => title,
        _ => plain_text(text_of(activity, "text").unwrap_or_default()),
    };
    let attachments = attachments_of(activity);
    if text.is_empty() && attachments.is_empty() && choice.is_none() {
        return parsed(InboundEvent::Ignored("an empty message"));
    }
    parsed(InboundEvent::Message(Box::new(InboundMessage {
        id: format!("{conversation_id}:{activity_id}"),
        chat_id: conversation_id.to_owned(),
        chat_kind,
        customer: Customer { id: sender.to_owned(), name: text_of(from, "name").map(str::to_owned) },
        text,
        attachments,
        // The card message a tap came from, to mark it as answered.
        reference: choice.as_ref().and_then(|_| text_of(activity, "replyToId")).map(str::to_owned),
        choice,
        timestamp_ms: None,
        unsupported: None,
    })))
}

/// An `Action.Submit` that sends the button's id back.
fn submit(button: &Button) -> Json {
    json!({
        "type": "Action.Submit",
        "title": crate::text::truncate(&button.title, 80),
        "data": { CHOICE_KEY: button.id, "graphrt_title": button.title },
    })
}

/// An attachment of an Adaptive Card with only `buttons`, for a question.
pub fn buttons_card(buttons: &[Button]) -> Option<Json> {
    if buttons.is_empty() {
        return None;
    }
    let card = json!({
        "type": "AdaptiveCard",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "version": CARD_VERSION,
        "body": [],
        "actions": buttons.iter().map(submit).collect::<Vec<_>>(),
    });
    Some(json!({ "contentType": "application/vnd.microsoft.card.adaptive", "content": card }))
}

/// An attachment of `card` as Teams draws it, its submit actions and inputs
/// replaced by `choices` as buttons. `None` when buttons cannot stand for its
/// answers (a text field, two inputs): send its fallback text then.
pub fn card_attachment(card: &Json, choices: &[Button]) -> Option<Json> {
    if !crate::card::buttons_can_answer(card, choices.len()) {
        return None;
    }
    let mut drawn = crate::card::without_answers(card);
    let object = drawn.as_object_mut()?;
    let mut actions = object.get("actions").and_then(Json::as_array).cloned().unwrap_or_default();
    actions.extend(choices.iter().map(submit));
    object.insert("actions".to_owned(), Json::Array(actions));
    object.insert("version".to_owned(), json!(CARD_VERSION));
    object.entry("$schema").or_insert_with(|| json!("http://adaptivecards.io/schemas/adaptive-card.json"));
    Some(json!({ "contentType": "application/vnd.microsoft.card.adaptive", "content": drawn }))
}

#[cfg(feature = "client")]
pub use client::{MAX_CONSENT_FILE_BYTES, TeamsBot, TeamsKeys};

#[cfg(feature = "client")]
mod client {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, PoisonError, RwLock};
    use std::time::{Duration, Instant};

    use serde_json::{Value as Json, json};

    use super::{
        SigningKey, TokenRefusal, TokenRules, buttons_card, card_attachment, parse_keys, text_of, verify_token,
    };
    use crate::channel::{BoxFuture, Channel, Download, OutboundFile, RichMessage, RichPart};
    use crate::http::{
        self, Answer, CALL_TIMEOUT, MAX_ANSWER_BYTES, TRANSFER_TIMEOUT, form, percent_encode, safe_file_name,
    };
    use crate::model::{Attachment, Button, ChannelError, ChannelKind, InboundMessage, Secret};
    use crate::text;

    const KIND: ChannelKind = ChannelKind::Teams;
    /// Where Microsoft Entra issues tokens.
    pub const LOGIN_BASE: &str = "https://login.microsoftonline.com";
    /// The service address for a conversation the bot has not heard from since it started.
    pub const DEFAULT_SERVICE_URL: &str = "https://smba.trafficmanager.net/teams/";
    /// Service addresses remembered at once.
    const MAX_SERVICE_URLS: usize = 20_000;
    /// The least time between two fetches of the keys.
    const MIN_REFETCH: Duration = Duration::from_secs(5 * 60);
    /// Keys are fetched again after this long, whatever happened.
    const KEYS_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
    /// Most text of one message, in UTF-16 units.
    const TEXT_UNITS: usize = 8000;
    const RETRIES: usize = 2;
    const MAX_RETRY_AFTER_SECS: u64 = 30;
    /// Largest file offered through a consent card.
    pub const MAX_CONSENT_FILE_BYTES: usize = 50 * 1024 * 1024;
    /// Files waiting for a person to accept them, and their bytes together.
    const MAX_PENDING_UPLOADS: usize = 64;
    const MAX_PENDING_BYTES: usize = 200 * 1024 * 1024;
    /// How long an offered file waits for its answer.
    const UPLOAD_WAIT: Duration = Duration::from_secs(60 * 60);

    /// A file offered with a consent card, until the person answers.
    #[derive(Debug)]
    struct PendingUpload {
        conversation_id: String,
        file: OutboundFile,
        offered: Instant,
    }

    fn bad_answer(method: &str, reason: &str) -> ChannelError {
        ChannelError::BadAnswer { channel: KIND, method: method.to_owned(), reason: reason.to_owned() }
    }

    fn refusal(method: &str, answer: &Answer) -> ChannelError {
        let body = answer.json();
        let description = body
            .get("error")
            .and_then(|error| error.get("message").and_then(Json::as_str).or_else(|| error.as_str()))
            .or_else(|| text_of(&body, "error_description"))
            .or_else(|| text_of(&body, "message"))
            .unwrap_or("no description");
        ChannelError::Api {
            channel: KIND,
            method: method.to_owned(),
            status: answer.status,
            code: None,
            description: text::truncate(description, 300),
            retry_after: answer.retry_after,
        }
    }

    /// The connector's signing keys, fetched from its OpenID configurations and
    /// fetched again when a token names a key they do not have.
    #[derive(Debug)]
    pub struct TeamsKeys {
        http: reqwest::Client,
        configurations: Vec<String>,
        keys: RwLock<Arc<Vec<SigningKey>>>,
        fetched: tokio::sync::Mutex<Option<Instant>>,
    }

    impl TeamsKeys {
        /// Keys from `configurations` (see [`super::openid_configurations`]),
        /// fetched on first use.
        pub fn new(http: reqwest::Client, configurations: Vec<String>) -> Self {
            Self {
                http,
                configurations,
                keys: RwLock::new(Arc::new(Vec::new())),
                fetched: tokio::sync::Mutex::new(None),
            }
        }

        fn current(&self) -> Arc<Vec<SigningKey>> {
            Arc::clone(&self.keys.read().unwrap_or_else(PoisonError::into_inner))
        }

        async fn get_json(&self, url: &str, what: &str) -> Result<Json, ChannelError> {
            let local = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost");
            if !url.starts_with("https://") && !local {
                return Err(bad_answer(what, "an address that is not https"));
            }
            let answer = http::send(KIND, self.http.get(url), CALL_TIMEOUT, MAX_ANSWER_BYTES).await?;
            if answer.status != 200 {
                return Err(refusal(what, &answer));
            }
            Ok(answer.json())
        }

        /// Fetch the keys again unless that happened in the last five minutes.
        /// Old keys stay when the fetch fails.
        ///
        /// @throws ChannelError - When no configuration gave a key.
        pub async fn refresh(&self) -> Result<(), ChannelError> {
            let mut fetched = self.fetched.lock().await;
            if fetched.is_some_and(|at| at.elapsed() < MIN_REFETCH) {
                return Ok(());
            }
            *fetched = Some(Instant::now());
            let mut keys = Vec::new();
            let mut last_error = None;
            for configuration in &self.configurations {
                let found = async {
                    let document = self.get_json(configuration, "OpenID configuration").await?;
                    let jwks_uri = text_of(&document, "jwks_uri")
                        .ok_or_else(|| bad_answer("OpenID configuration", "no jwks_uri"))?;
                    Ok::<_, ChannelError>(parse_keys(&self.get_json(jwks_uri, "signing keys").await?))
                };
                match found.await {
                    Ok(found) => keys.extend(found),
                    Err(error) => last_error = Some(error),
                }
            }
            if keys.is_empty() {
                return Err(last_error.unwrap_or_else(|| bad_answer("signing keys", "no RSA signing key")));
            }
            *self.keys.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(keys);
            Ok(())
        }

        /// Check a delivery's `Authorization` header, fetching the keys when
        /// they are missing, old, or lack the token's key.
        ///
        /// @throws TokenRefusal - When the token does not pass.
        pub async fn verify(&self, authorization: Option<&str>, rules: &TokenRules<'_>) -> Result<(), TokenRefusal> {
            let stale = self.fetched.lock().await.is_none_or(|at| at.elapsed() > KEYS_MAX_AGE);
            if (stale || self.current().is_empty())
                && let Err(error) = self.refresh().await
            {
                eprintln!("warning: teams: cannot fetch the Bot Framework signing keys: {error}");
            }
            match verify_token(authorization, &self.current(), rules) {
                Err(TokenRefusal::UnknownKey) => {
                    // The keys rotate every few weeks.
                    let _refreshed = self.refresh().await;
                    verify_token(authorization, &self.current(), rules)
                }
                other => other,
            }
        }
    }

    /// The bot's access token and when it runs out.
    #[derive(Debug)]
    struct AccessToken {
        token: Secret,
        until: Instant,
    }

    /// One Azure Bot, through its Microsoft app id and client secret.
    #[derive(Debug)]
    pub struct TeamsBot {
        http: reqwest::Client,
        app_id: String,
        app_password: Secret,
        /// The tenant of a single tenant bot, also needed to open chats.
        tenant: Option<String>,
        login_base: String,
        default_service_url: String,
        /// The `serviceUrl` of each conversation, from verified activities.
        service_urls: Mutex<HashMap<String, String>>,
        /// The 1:1 conversation of each person the bot opened one with.
        direct_chats: Mutex<HashMap<String, String>>,
        /// Files offered with a consent card, by upload id.
        pending_uploads: Mutex<HashMap<String, PendingUpload>>,
        token: tokio::sync::Mutex<Option<AccessToken>>,
    }

    impl TeamsBot {
        /// The bot `app_id` with its client secret `app_password`. `tenant` is
        /// the Entra tenant of a single tenant bot (the kind Azure creates now).
        ///
        /// @throws ChannelError - `Invalid` when the app id is not a GUID.
        pub fn new(
            http: reqwest::Client,
            app_id: &str,
            app_password: Secret,
            tenant: Option<String>,
        ) -> Result<Self, ChannelError> {
            let is_guid = |id: &str| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
            if !is_guid(app_id) {
                return Err(ChannelError::Invalid(
                    "the Teams app id is not one (expected the Microsoft App ID of the Azure Bot, a GUID)".to_owned(),
                ));
            }
            if tenant.as_deref().is_some_and(|tenant| !is_guid(tenant)) {
                return Err(ChannelError::Invalid("the Teams tenant id is not a GUID".to_owned()));
            }
            Ok(Self {
                http,
                app_id: app_id.to_owned(),
                app_password,
                tenant,
                login_base: LOGIN_BASE.to_owned(),
                default_service_url: DEFAULT_SERVICE_URL.to_owned(),
                service_urls: Mutex::new(HashMap::new()),
                direct_chats: Mutex::new(HashMap::new()),
                pending_uploads: Mutex::new(HashMap::new()),
                token: tokio::sync::Mutex::new(None),
            })
        }

        /// Get tokens from another login server (a fake in tests).
        pub fn with_login_base(mut self, base: &str) -> Self {
            self.login_base = base.trim_end_matches('/').to_owned();
            self
        }

        /// Send to another service address when a conversation's own is unknown.
        pub fn with_service_url(mut self, url: &str) -> Self {
            self.default_service_url = url.to_owned();
            self
        }

        pub fn app_id(&self) -> &str {
            &self.app_id
        }

        pub fn tenant(&self) -> Option<&str> {
            self.tenant.as_deref()
        }

        /// Remember where answers to `conversation_id` go. Call with the
        /// `serviceUrl` of an activity whose token passed [`super::verify_token`] only.
        pub fn remember_service_url(&self, conversation_id: &str, service_url: &str) {
            if !service_url.starts_with("https://") && !service_url.starts_with("http://127.0.0.1") {
                return;
            }
            let mut urls = self.service_urls.lock().unwrap_or_else(PoisonError::into_inner);
            if urls.len() >= MAX_SERVICE_URLS && !urls.contains_key(conversation_id) {
                urls.clear();
            }
            urls.insert(conversation_id.to_owned(), service_url.to_owned());
        }

        fn service_url(&self, conversation_id: &str) -> String {
            let urls = self.service_urls.lock().unwrap_or_else(PoisonError::into_inner);
            let url = urls.get(conversation_id).cloned().unwrap_or_else(|| self.default_service_url.clone());
            format!("{}/", url.trim_end_matches('/'))
        }

        /// A token for the Bot Connector, from the cache while it is good.
        async fn access_token(&self) -> Result<Secret, ChannelError> {
            let mut cached = self.token.lock().await;
            if let Some(token) = cached.as_ref().filter(|token| token.until > Instant::now()) {
                return Ok(token.token.clone());
            }
            let tenant = self.tenant.as_deref().unwrap_or("botframework.com");
            let url = format!("{}/{tenant}/oauth2/v2.0/token", self.login_base);
            let body = form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.app_id),
                ("client_secret", self.app_password.expose()),
                ("scope", "https://api.botframework.com/.default"),
            ]);
            let request = self
                .http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body);
            let answer = http::send(KIND, request, CALL_TIMEOUT, MAX_ANSWER_BYTES).await?;
            if answer.status != 200 {
                let mut refused = refusal("token", &answer);
                if let ChannelError::Api { description, .. } = &mut refused {
                    *description = self.app_password.redact(description);
                }
                return Err(refused);
            }
            let body = answer.json();
            let token = text_of(&body, "access_token").ok_or_else(|| bad_answer("token", "no access_token"))?;
            let lifetime = body.get("expires_in").and_then(Json::as_u64).unwrap_or(3600);
            let token = Secret::new(token);
            *cached = Some(AccessToken {
                token: token.clone(),
                until: Instant::now() + Duration::from_secs(lifetime.saturating_sub(300).max(60)),
            });
            Ok(token)
        }

        /// Send `body` to `url` as the bot. A `429` with a short wait is waited out.
        async fn call(
            &self,
            method: reqwest::Method,
            url: &str,
            what: &str,
            body: &Json,
        ) -> Result<Json, ChannelError> {
            let body = serde_json::to_vec(body).map_err(|e| ChannelError::Invalid(e.to_string()))?;
            let mut attempt = 0;
            loop {
                let token = self.access_token().await?;
                let request = self
                    .http
                    .request(method.clone(), url)
                    .bearer_auth(token.expose())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone());
                let answer = http::send(KIND, request, CALL_TIMEOUT, MAX_ANSWER_BYTES).await?;
                if (200..300).contains(&answer.status) {
                    return Ok(answer.json());
                }
                if answer.status == 401 {
                    // A token revoked early: the next call gets a new one.
                    *self.token.lock().await = None;
                }
                match answer.retry_after {
                    Some(wait) if answer.status == 429 && attempt < RETRIES && wait <= MAX_RETRY_AFTER_SECS => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
                    }
                    _ => return Err(refusal(what, &answer)),
                }
            }
        }

        fn activities_url(&self, conversation_id: &str) -> String {
            format!(
                "{}v3/conversations/{}/activities",
                self.service_url(conversation_id),
                percent_encode(conversation_id)
            )
        }

        /// Send one activity to `conversation_id`.
        ///
        /// @returns The activity's id.
        pub async fn send_activity(&self, conversation_id: &str, activity: &Json) -> Result<String, ChannelError> {
            let sent =
                self.call(reqwest::Method::POST, &self.activities_url(conversation_id), "send", activity).await?;
            Ok(text_of(&sent, "id").unwrap_or_default().to_owned())
        }

        fn message(text: &str, attachments: Vec<Json>) -> Json {
            let mut activity = json!({ "type": "message", "textFormat": "markdown" });
            if let Some(object) = activity.as_object_mut() {
                if !text.trim().is_empty() {
                    object.insert("text".to_owned(), json!(text));
                }
                if !attachments.is_empty() {
                    object.insert("attachments".to_owned(), Json::Array(attachments));
                }
            }
            activity
        }

        async fn send_parts(
            &self,
            chat_id: &str,
            markdown: &str,
            buttons: &[Button],
        ) -> Result<Vec<String>, ChannelError> {
            let parts = text::split(markdown, TEXT_UNITS);
            let mut sent = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                let last = index + 1 == parts.len();
                let card = if last { buttons_card(buttons) } else { None };
                sent.push(self.send_activity(chat_id, &Self::message(part, card.into_iter().collect())).await?);
            }
            if parts.is_empty()
                && let Some(card) = buttons_card(buttons)
            {
                sent.push(self.send_activity(chat_id, &Self::message("", vec![card])).await?);
            }
            Ok(sent)
        }

        async fn rich(&self, chat_id: &str, message: &RichMessage) -> Result<Vec<String>, ChannelError> {
            let mut sent = Vec::new();
            let mut text_so_far = String::new();
            for part in &message.parts {
                match part {
                    RichPart::Markdown(markdown) => {
                        if !text_so_far.is_empty() {
                            text_so_far.push_str("\n\n");
                        }
                        text_so_far.push_str(markdown.trim());
                    }
                    RichPart::Card { card, fallback, choices } => match card_attachment(card, choices) {
                        Some(attachment) => {
                            // Text before a card goes first, in its own message.
                            for part in text::split(&std::mem::take(&mut text_so_far), TEXT_UNITS) {
                                sent.push(self.send_activity(chat_id, &Self::message(&part, Vec::new())).await?);
                            }
                            sent.push(self.send_activity(chat_id, &Self::message("", vec![attachment])).await?);
                        }
                        None => {
                            sent.extend(self.send_parts(chat_id, &std::mem::take(&mut text_so_far), &[]).await?);
                            sent.extend(self.send_parts(chat_id, fallback, choices).await?);
                        }
                    },
                }
            }
            sent.extend(self.send_parts(chat_id, &text_so_far, &message.buttons).await?);
            Ok(sent)
        }

        /// Open (or find) the 1:1 chat with `user_id`, an Entra object id or a
        /// `29:` id. The bot must be installed for that person. A conversation
        /// id is returned as it is.
        pub async fn open_direct_chat(&self, user_id: &str) -> Result<String, ChannelError> {
            // A conversation id (a channel `19:...`, a personal chat `a:...`) is a chat already.
            if user_id.starts_with("19:") || user_id.starts_with("a:") {
                return Ok(user_id.to_owned());
            }
            if let Some(known) = self.direct_chats.lock().unwrap_or_else(PoisonError::into_inner).get(user_id) {
                return Ok(known.clone());
            }
            let Some(tenant) = self.tenant.as_deref() else {
                return Err(ChannelError::Invalid(
                    "set the tenant id of the Teams bot to message people who have not written to it".to_owned(),
                ));
            };
            let body = json!({
                "isGroup": false,
                "bot": { "id": format!("28:{}", self.app_id) },
                "members": [{ "id": user_id }],
                "tenantId": tenant,
                "channelData": { "tenant": { "id": tenant } },
            });
            let url = format!("{}v3/conversations", self.service_url(""));
            let opened = self.call(reqwest::Method::POST, &url, "create conversation", &body).await?;
            let id = text_of(&opened, "id").ok_or_else(|| bad_answer("create conversation", "no conversation id"))?;
            let mut chats = self.direct_chats.lock().unwrap_or_else(PoisonError::into_inner);
            if chats.len() >= MAX_SERVICE_URLS {
                chats.clear();
            }
            chats.insert(user_id.to_owned(), id.to_owned());
            Ok(id.to_owned())
        }

        /// Remember the 1:1 chat a person wrote from, so the bot can write to them later.
        pub fn remember_direct_chat(&self, user_id: &str, conversation_id: &str) {
            let mut chats = self.direct_chats.lock().unwrap_or_else(PoisonError::into_inner);
            if chats.len() >= MAX_SERVICE_URLS && !chats.contains_key(user_id) {
                chats.clear();
            }
            chats.insert(user_id.to_owned(), conversation_id.to_owned());
        }

        /// Replace the card a person tapped with what they chose, so its buttons
        /// cannot be tapped twice.
        async fn mark_tapped(&self, message: &InboundMessage) -> Result<(), ChannelError> {
            let Some(card_id) = message.reference.as_deref() else { return Ok(()) };
            let url = format!("{}/{}", self.activities_url(&message.chat_id), percent_encode(card_id));
            let activity = json!({ "type": "message", "text": format!("**{}**", message.text) });
            self.call(reqwest::Method::PUT, &url, "update", &activity).await.map(|_| ())
        }

        async fn fetch(&self, attachment: &Attachment, max_bytes: u64) -> Result<Download, ChannelError> {
            let url = attachment.file_id.as_str();
            let local = url.starts_with("http://127.0.0.1");
            if !url.starts_with("https://") && !local {
                return Err(bad_answer("file download", "an address that is not https"));
            }
            let mut request = self.http.get(url);
            // A picture pasted in a chat lives with the connector and needs the bot's token.
            // A shared file has a download address of its own, which needs none.
            let is_connector = self
                .service_urls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .chain(std::iter::once(&self.default_service_url))
                .any(|service| {
                    let origin = service.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
                    url.starts_with(&format!("{origin}/"))
                });
            if is_connector {
                request = request.bearer_auth(self.access_token().await?.expose());
            }
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, max_bytes).await?;
            if answer.status != 200 {
                return Err(refusal("file download", &answer));
            }
            let mime_type = attachment.mime_type.clone().or(answer.content_type.clone());
            Ok(Download { bytes: answer.body, mime_type, name: attachment.name.clone() })
        }

        /// A random id for an upload: unguessable, so a consent answer cannot
        /// name another person's file.
        fn upload_id() -> Result<String, ChannelError> {
            let mut bytes = [0_u8; 16];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
                .map_err(|_| ChannelError::Invalid("no random numbers for an upload id".to_owned()))?;
            Ok(crate::sign::to_hex(&bytes))
        }

        /// Offer `file` in the 1:1 chat `chat_id` with a file consent card: the
        /// person accepts, Teams hands over a OneDrive upload address, and
        /// [`TeamsBot::answer_file_consent`] puts the bytes there.
        async fn offer_file(&self, chat_id: &str, file: &OutboundFile) -> Result<String, ChannelError> {
            if file.bytes.len() > MAX_CONSENT_FILE_BYTES {
                return Err(ChannelError::TooLarge {
                    limit: u64::try_from(MAX_CONSENT_FILE_BYTES).unwrap_or(u64::MAX),
                });
            }
            let id = Self::upload_id()?;
            {
                let mut pending = self.pending_uploads.lock().unwrap_or_else(PoisonError::into_inner);
                pending.retain(|_, upload| upload.offered.elapsed() < UPLOAD_WAIT);
                let held: usize = pending.values().map(|upload| upload.file.bytes.len()).sum();
                if pending.len() >= MAX_PENDING_UPLOADS || held + file.bytes.len() > MAX_PENDING_BYTES {
                    return Err(ChannelError::Invalid("too many files wait for an answer in Teams".to_owned()));
                }
                pending.insert(
                    id.clone(),
                    PendingUpload { conversation_id: chat_id.to_owned(), file: file.clone(), offered: Instant::now() },
                );
            }
            let name = safe_file_name(&file.name);
            let context = json!({ super::UPLOAD_KEY: id });
            let card = json!({
                "contentType": "application/vnd.microsoft.teams.card.file.consent",
                "name": name,
                "content": {
                    "description": file.caption.as_deref().map(|caption| text::truncate(caption, 200)).unwrap_or_else(|| name.clone()),
                    "sizeInBytes": file.bytes.len(),
                    "acceptContext": context,
                    "declineContext": context,
                },
            });
            let sent = self.send_activity(chat_id, &Self::message("", vec![card])).await;
            if sent.is_err() {
                self.pending_uploads.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            }
            sent
        }

        /// Finish a file consent: upload the bytes and send the file card when
        /// the person accepted, and replace the consent card either way.
        ///
        /// @throws ChannelError - When the file is unknown or expired, belongs to
        ///   another chat, or the upload or the card was refused.
        pub async fn answer_file_consent(&self, consent: &super::FileConsent) -> Result<(), ChannelError> {
            let pending = {
                let mut pending = self.pending_uploads.lock().unwrap_or_else(PoisonError::into_inner);
                // Only the chat the file was offered in can take it, and only while it waits.
                let matches = pending.get(&consent.upload_id).is_some_and(|upload| {
                    upload.conversation_id == consent.conversation_id && upload.offered.elapsed() < UPLOAD_WAIT
                });
                if matches { pending.remove(&consent.upload_id) } else { None }
            };
            let Some(pending) = pending else {
                return Err(ChannelError::Invalid("the file of this card is gone: ask for it again".to_owned()));
            };
            let name = safe_file_name(&pending.file.name);
            let said = if consent.accepted {
                self.put_upload(consent, &pending.file).await?;
                let card = json!({
                    "contentType": "application/vnd.microsoft.teams.card.file.info",
                    "contentUrl": consent.content_url,
                    "name": consent.name.clone().unwrap_or_else(|| name.clone()),
                    "content": { "uniqueId": consent.unique_id, "fileType": consent.file_type },
                });
                self.send_activity(&consent.conversation_id, &Self::message("", vec![card])).await?;
                format!("Sent: {name}")
            } else {
                format!("Not sent: {name}")
            };
            if let Some(card_id) = &consent.card_id {
                let url = format!("{}/{}", self.activities_url(&consent.conversation_id), percent_encode(card_id));
                let replaced = json!({ "type": "message", "text": said });
                // Best effort: the file is there either way.
                let _replaced = self.call(reqwest::Method::PUT, &url, "update", &replaced).await;
            }
            Ok(())
        }

        /// Put the bytes into the OneDrive upload session of `consent`, in one request.
        async fn put_upload(&self, consent: &super::FileConsent, file: &OutboundFile) -> Result<(), ChannelError> {
            let url = consent.upload_url.as_deref().unwrap_or_default();
            if !url.starts_with("https://") && !url.starts_with("http://127.0.0.1") {
                return Err(bad_answer("file upload", "an upload address that is not https"));
            }
            let length = file.bytes.len();
            let range = format!("bytes 0-{}/{length}", length.saturating_sub(1));
            // The session address carries its own authorization: no token goes with it.
            let request = self
                .http
                .put(url)
                .header(reqwest::header::CONTENT_RANGE, range)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(file.bytes.clone());
            let answer = http::send(KIND, request, TRANSFER_TIMEOUT, MAX_ANSWER_BYTES).await?;
            if !(200..300).contains(&answer.status) {
                return Err(refusal("file upload", &answer));
            }
            Ok(())
        }

        async fn upload(&self, chat_id: &str, file: &OutboundFile) -> Result<String, ChannelError> {
            // A picture fits inline. Any other file goes through a consent card in a
            // 1:1 chat. A channel or a group chat takes no file from a bot: the host
            // shares a link instead.
            const MAX_INLINE_BYTES: usize = 1024 * 1024;
            if file.is_image() && file.bytes.len() <= MAX_INLINE_BYTES {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(&file.bytes);
                let attachment = json!({
                    "contentType": file.mime_type,
                    "contentUrl": format!("data:{};base64,{data}", file.mime_type),
                    "name": safe_file_name(&file.name),
                });
                let caption = file.caption.as_deref().unwrap_or_default();
                return self.send_activity(chat_id, &Self::message(caption, vec![attachment])).await;
            }
            if chat_id.starts_with("a:") {
                return self.offer_file(chat_id, file).await;
            }
            Err(ChannelError::Invalid(format!(
                "Teams takes files from a bot in 1:1 chats only: `{}` needs a link here",
                file.name
            )))
        }
    }

    impl Channel for TeamsBot {
        fn kind(&self) -> ChannelKind {
            KIND
        }

        fn send_text<'a>(
            &'a self,
            chat_id: &'a str,
            markdown: &'a str,
            buttons: &'a [Button],
        ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>> {
            Box::pin(self.send_parts(chat_id, markdown, buttons))
        }

        fn send_file<'a>(
            &'a self,
            chat_id: &'a str,
            file: &'a OutboundFile,
        ) -> BoxFuture<'a, Result<String, ChannelError>> {
            Box::pin(self.upload(chat_id, file))
        }

        fn typing<'a>(&'a self, message: &'a InboundMessage) -> BoxFuture<'a, Result<(), ChannelError>> {
            Box::pin(async move {
                if message.choice.is_some() {
                    // Best effort: a card that cannot be changed still has its answer.
                    let _marked = self.mark_tapped(message).await;
                }
                self.send_activity(&message.chat_id, &json!({ "type": "typing" })).await.map(|_| ())
            })
        }

        fn draws_cards(&self) -> bool {
            true
        }

        fn send_rich<'a>(
            &'a self,
            chat_id: &'a str,
            message: &'a RichMessage,
        ) -> BoxFuture<'a, Result<Vec<String>, ChannelError>> {
            Box::pin(self.rich(chat_id, message))
        }

        fn direct_chat<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<String, ChannelError>> {
            Box::pin(self.open_direct_chat(user_id))
        }

        fn download<'a>(
            &'a self,
            attachment: &'a Attachment,
            max_bytes: u64,
        ) -> BoxFuture<'a, Result<Download, ChannelError>> {
            Box::pin(self.fetch(attachment, max_bytes))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const APP_ID: &str = "00000000-1111-2222-3333-444444444444";

        #[test]
        fn the_app_id_and_tenant_must_be_guids_and_the_secret_never_prints() {
            let http = crate::http::test_client();
            let bot = TeamsBot::new(http.clone(), APP_ID, Secret::new("client-secret-value"), None);
            assert!(bot.as_ref().is_ok_and(|bot| !format!("{bot:?}").contains("client-secret-value")));
            assert!(TeamsBot::new(http.clone(), "my-bot", Secret::new("x"), None).is_err());
            assert!(TeamsBot::new(http, APP_ID, Secret::new("x"), Some("contoso".to_owned())).is_err());
        }

        #[test]
        fn answers_go_to_the_conversation_s_own_service_address() {
            let bot = TeamsBot::new(crate::http::test_client(), APP_ID, Secret::new("x"), None)
                .unwrap_or_else(|e| unreachable!("{e}"));
            assert_eq!(
                bot.activities_url("a:1"),
                "https://smba.trafficmanager.net/teams/v3/conversations/a%3A1/activities"
            );
            bot.remember_service_url("a:1", "https://smba.trafficmanager.net/emea/tenant");
            assert!(bot.activities_url("a:1").starts_with("https://smba.trafficmanager.net/emea/tenant/v3/"));
            bot.remember_service_url("a:2", "http://evil.example/");
            assert!(bot.activities_url("a:2").starts_with(DEFAULT_SERVICE_URL), "only https addresses are kept");
        }
    }
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    use super::*;

    const APP_ID: &str = "00000000-1111-2222-3333-444444444444";
    const NOW: u64 = 1_800_000_000;
    const SERVICE: &str = "https://smba.trafficmanager.net/emea/";

    fn key_pair() -> RsaKeyPair {
        RsaKeyPair::from_der(include_bytes!("../fixtures/teams-test-key.der")).unwrap_or_else(|e| unreachable!("{e}"))
    }

    fn jwks(pair: &RsaKeyPair, endorsements: &[&str]) -> Json {
        let public = ring::signature::RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
        json!({ "keys": [{
            "kty": "RSA", "use": "sig", "kid": "key-1",
            "n": URL_SAFE_NO_PAD.encode(&public.n),
            "e": URL_SAFE_NO_PAD.encode(&public.e),
            "endorsements": endorsements,
        }]})
    }

    fn token(pair: &RsaKeyPair, header: &Json, claims: &Json) -> String {
        let signed =
            format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut signature = vec![0; pair.public().modulus_len()];
        pair.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signed.as_bytes(), &mut signature)
            .unwrap_or_else(|e| unreachable!("{e}"));
        format!("Bearer {signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn claims() -> Json {
        json!({ "iss": CONNECTOR_ISSUER, "aud": APP_ID, "exp": NOW + 3600, "nbf": NOW - 10, "serviceurl": SERVICE })
    }

    fn check(authorization: &str, keys: &[SigningKey]) -> Result<(), TokenRefusal> {
        let issuers = issuers(None);
        let rules = TokenRules {
            app_id: APP_ID,
            issuers: &issuers,
            service_url: Some("https://smba.trafficmanager.net/emea"),
            channel_id: Some("msteams"),
            now_secs: NOW,
        };
        verify_token(Some(authorization), keys, &rules)
    }

    #[test]
    fn a_connector_token_passes_and_every_wrong_part_is_refused() {
        let pair = key_pair();
        let keys = parse_keys(&jwks(&pair, &["msteams"]));
        let header = json!({ "alg": "RS256", "kid": "key-1", "typ": "JWT" });
        assert_eq!(check(&token(&pair, &header, &claims()), &keys), Ok(()));

        let with = |field: &str, value: Json| {
            let mut changed = claims();
            changed[field] = value;
            check(&token(&pair, &header, &changed), &keys)
        };
        assert_eq!(with("aud", json!("another-bot")), Err(TokenRefusal::Audience));
        assert_eq!(with("iss", json!("https://evil.example")), Err(TokenRefusal::Issuer));
        assert_eq!(with("exp", json!(NOW - 400)), Err(TokenRefusal::Time));
        assert_eq!(with("nbf", json!(NOW + 400)), Err(TokenRefusal::Time));
        assert_eq!(with("serviceurl", json!("https://evil.example/")), Err(TokenRefusal::ServiceUrl));

        let other_kid = json!({ "alg": "RS256", "kid": "key-2" });
        assert_eq!(check(&token(&pair, &other_kid, &claims()), &keys), Err(TokenRefusal::UnknownKey));
        let hmac = json!({ "alg": "HS256", "kid": "key-1" });
        assert_eq!(check(&token(&pair, &hmac, &claims()), &keys), Err(TokenRefusal::Algorithm));
        let good = token(&pair, &header, &claims());
        let tampered = good.replacen('.', ".e30", 1);
        assert!(check(&tampered, &keys).is_err());
        assert_eq!(check("Basic abc", &keys), Err(TokenRefusal::Missing));
        assert_eq!(check("Bearer not-a-token", &keys), Err(TokenRefusal::Malformed));
        // A key endorsed for another channel signs nothing for Teams.
        let skype = parse_keys(&jwks(&pair, &["skype"]));
        assert_eq!(check(&good, &skype), Err(TokenRefusal::Endorsement));
    }

    #[test]
    fn a_single_tenant_bot_also_accepts_its_tenant_issuers() {
        let tenant = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let accepted = issuers(Some(tenant));
        assert!(accepted.contains(&format!("https://login.microsoftonline.com/{tenant}/v2.0")));
        assert_eq!(openid_configurations(Some(tenant)).len(), 2);
        assert_eq!(issuers(None), [CONNECTOR_ISSUER]);
    }

    fn message(parsed: ParsedActivity) -> InboundMessage {
        match parsed.event {
            InboundEvent::Message(message) => *message,
            other => unreachable!("expected a message, got {other:?}"),
        }
    }

    fn activity(conversation: Json, text: &str) -> Json {
        json!({ "type": "message", "id": "1700000000001", "channelId": "msteams", "serviceUrl": SERVICE,
            "from": { "id": "29:1abc", "aadObjectId": "a1b2c3d4-0000-0000-0000-000000000001", "name": "Rina" },
            "conversation": conversation, "recipient": { "id": "28:bot" }, "text": text,
            "channelData": { "tenant": { "id": "tenant-1" } } })
    }

    #[test]
    fn a_web_chat_delivery_that_names_its_own_sender_is_not_read() {
        let mut forged = activity(json!({ "id": "a:1personal", "conversationType": "personal" }), "hi");
        forged["channelId"] = json!("webchat");
        forged["from"] = json!({ "id": "a1b2c3d4-0000-0000-0000-000000000001" });
        assert!(matches!(parse_activity(&forged).event, InboundEvent::Ignored("not from Microsoft Teams")));
        if let Some(fields) = forged.as_object_mut() {
            fields.remove("channelId");
        }
        assert!(matches!(parse_activity(&forged).event, InboundEvent::Ignored(_)));
    }

    #[test]
    fn a_personal_chat_and_a_channel_thread_are_conversations_of_their_own() {
        let personal =
            parse_activity(&activity(json!({ "id": "a:1personal", "conversationType": "personal" }), "Halo &amp; hi"));
        assert_eq!((personal.service_url.as_deref(), personal.tenant_id.as_deref()), (Some(SERVICE), Some("tenant-1")));
        let personal = message(personal);
        assert_eq!((personal.chat_id.as_str(), personal.chat_kind), ("a:1personal", ChatKind::Private));
        assert_eq!(personal.customer.id, "a1b2c3d4-0000-0000-0000-000000000001", "the Entra object id");
        assert_eq!(personal.text, "Halo & hi");

        let thread = "19:abc@thread.tacv2;messageid=1700000000000";
        let mention =
            activity(json!({ "id": thread, "conversationType": "channel" }), "<at>Support Bot</at> where is my order?");
        let mention = message(parse_activity(&mention));
        assert_eq!((mention.chat_id.as_str(), mention.chat_kind), (thread, ChatKind::Group));
        assert_eq!(mention.text, "where is my order?");
    }

    #[test]
    fn a_card_tap_is_a_choice_and_bots_and_updates_are_ignored() {
        let mut tap = activity(json!({ "id": "a:1", "conversationType": "personal" }), "");
        tap["value"] = json!({ CHOICE_KEY: "2", "graphrt_title": "Reject" });
        tap["replyToId"] = json!("card-9");
        let tap = message(parse_activity(&tap));
        assert_eq!(
            (tap.choice.as_deref(), tap.text.as_str(), tap.reference.as_deref()),
            (Some("2"), "Reject", Some("card-9"))
        );

        let mut bot = activity(json!({ "id": "a:1" }), "hi");
        bot["from"]["role"] = json!("bot");
        assert!(matches!(parse_activity(&bot).event, InboundEvent::Ignored(_)));
        let update = json!({ "type": "conversationUpdate", "serviceUrl": SERVICE });
        assert!(matches!(parse_activity(&update).event, InboundEvent::Ignored(_)));
    }

    #[test]
    fn files_shared_in_a_chat_are_attachments_and_the_html_copy_is_not() {
        let mut shared = activity(json!({ "id": "a:1", "conversationType": "personal" }), "");
        shared["attachments"] = json!([
            { "contentType": "application/vnd.microsoft.teams.file.download.info", "name": "faktur.pdf",
              "content": { "downloadUrl": "https://contoso.sharepoint.com/x", "fileType": "pdf" } },
            { "contentType": "image/png", "contentUrl": "https://smba.trafficmanager.net/emea/v3/attachments/1" },
            { "contentType": "text/html", "content": "<p>hi</p>" }]);
        let shared = message(parse_activity(&shared));
        let kinds: Vec<AttachmentKind> = shared.attachments.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, [AttachmentKind::Document, AttachmentKind::Image]);
    }

    #[test]
    fn a_card_keeps_its_content_and_gets_the_choices_as_buttons() {
        let card = json!({ "type": "AdaptiveCard", "version": "1.6", "body": [
            { "type": "TextBlock", "text": "Refund Rp 250.000?" },
            { "type": "Input.ChoiceSet", "id": "answer", "choices": [{ "title": "Yes", "value": "y" }, { "title": "No", "value": "n" }] }],
            "actions": [{ "type": "Action.Submit", "title": "Send" }, { "type": "Action.OpenUrl", "title": "Policy", "url": "https://e.com" }] });
        let choices = [Button { id: "1".into(), title: "Yes".into() }, Button { id: "2".into(), title: "No".into() }];
        let attachment = card_attachment(&card, &choices).unwrap_or_default();
        let content = &attachment["content"];
        assert_eq!(attachment["contentType"], "application/vnd.microsoft.card.adaptive");
        assert_eq!((content["version"].as_str(), content["body"].as_array().map(Vec::len)), (Some("1.5"), Some(1)));
        let titles: Vec<&str> =
            content["actions"].as_array().into_iter().flatten().filter_map(|a| a["title"].as_str()).collect();
        assert_eq!(titles, ["Policy", "Yes", "No"]);
        assert_eq!(content["actions"][2]["data"][CHOICE_KEY], "2");
        let form = json!({ "type": "AdaptiveCard", "body": [{ "type": "Input.Text", "id": "note" }] });
        assert_eq!(card_attachment(&form, &choices[..1]), None);
        assert!(buttons_card(&[]).is_none());
    }

    #[test]
    fn a_file_consent_answer_is_read_from_its_invoke() {
        let invoke = json!({ "type": "invoke", "name": "fileConsent/invoke", "id": "f1", "channelId": "msteams",
            "serviceUrl": SERVICE, "replyToId": "card-1",
            "from": { "id": "29:1abc", "aadObjectId": "aad-1" }, "conversation": { "id": "a:1", "conversationType": "personal" },
            "value": { "type": "fileUpload", "action": "accept", "context": { UPLOAD_KEY: "u1" },
                       "uploadInfo": { "name": "report.pdf", "uploadUrl": "https://contoso-my.sharepoint.com/upload/1",
                                       "contentUrl": "https://contoso-my.sharepoint.com/report.pdf", "uniqueId": "id-1", "fileType": "pdf" } } });
        let parsed = parse_activity(&invoke);
        assert!(matches!(parsed.event, InboundEvent::Ignored(_)));
        let consent = parsed.file_consent.unwrap_or_else(|| unreachable!("a consent"));
        assert!(consent.accepted);
        assert_eq!(
            (consent.upload_id.as_str(), consent.conversation_id.as_str(), consent.from.as_str()),
            ("u1", "a:1", "aad-1")
        );
        assert_eq!(consent.card_id.as_deref(), Some("card-1"));
        let mut declined = invoke;
        declined["value"]["action"] = json!("decline");
        assert!(!parse_activity(&declined).file_consent.is_some_and(|consent| consent.accepted));
    }

    #[test]
    fn mentions_and_markup_are_removed_from_the_text() {
        assert_eq!(plain_text("<at>Bot</at>&nbsp;hello <b>there</b>"), "hello there");
        assert_eq!(plain_text("<at>Bot"), "");
    }
}
