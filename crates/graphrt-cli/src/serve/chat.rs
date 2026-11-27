//! The chat page and widget of `graphrt serve` (`--chat WORKFLOW`, cargo
//! feature `web-chat`).
//!
//! | Method | Path | Answer |
//! | --- | --- | --- |
//! | GET | `/chat/` | the chat page of the first `--chat` workflow |
//! | GET | `/chat/{workflow}` | the chat page of that workflow |
//! | GET | `/chat/chat.js` | the page's script (client, UI blocks renderer and chat component) |
//! | GET | `/chat/embed.js` | the widget: one script tag on any site adds a chat bubble |
//! | GET | `/v1/chat/config` | what the page offers (title, workflows, who may chat); public |
//! | POST | `/v1/chat/guest` | a guest session (`--chat-access guests` only); public |
//!
//! The page is served from the same origin as the API, so it needs no CORS.
//! The widget runs on another site and does: list that site in `--cors-origin`.
//!
//! The page and scripts are built from `web/` and embedded in the binary
//! (`crates/graphrt-cli/assets/chat`). They load nothing from other hosts:
//! the page's `Content-Security-Policy` allows scripts and connections of its
//! own origin only.
//!
//! Who may chat is `--chat-access`: `key` (the page asks for the API key once),
//! `users` (the embedding page hands the widget a token that `--verify-jwt`
//! verifies) or `guests` (anyone, see [`super::guests`]).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use sha2::{Digest, Sha256};

use super::errors::error_response;
use super::guests::{Guests, is_guest_token};
use super::state::AppState;
use crate::args::{ChatAccess, ChatArgs};
use crate::error::CliError;

const PAGE: &[u8] = include_bytes!("../../assets/chat/index.html");
const PAGE_SCRIPT: &[u8] = include_bytes!("../../assets/chat/chat.js");
const WIDGET_SCRIPT: &[u8] = include_bytes!("../../assets/chat/embed.js");

/// A workflow the chat page offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatWorkflow {
    pub name: String,
    /// The start input that takes the message; `None` when it is `sys.query`.
    pub input: Option<String>,
}

impl ChatWorkflow {
    /// Parse `WORKFLOW` or `WORKFLOW=INPUT`.
    ///
    /// @throws String - When a part is empty.
    fn parse(spec: &str) -> Result<Self, String> {
        let (name, input) = match spec.split_once('=') {
            Some((name, input)) => (name, Some(input)),
            None => (spec, None),
        };
        if name.is_empty() || input.is_some_and(str::is_empty) {
            return Err(format!("--chat `{spec}`: expected WORKFLOW or WORKFLOW=INPUT"));
        }
        Ok(Self { name: name.to_owned(), input: input.map(str::to_owned) })
    }
}

/// One embedded file with its headers.
#[derive(Debug)]
struct Asset {
    bytes: &'static [u8],
    content_type: &'static str,
    /// A strong validator: the page changes only with the binary.
    etag: HeaderValue,
}

impl Asset {
    fn new(bytes: &'static [u8], content_type: &'static str) -> Self {
        let digest = Sha256::digest(bytes);
        let tag = format!("\"{}\"", crate::hex::to_hex(digest.get(..12).unwrap_or_default()));
        let etag = HeaderValue::from_str(&tag).unwrap_or_else(|_| HeaderValue::from_static("\"0\""));
        Self { bytes, content_type, etag }
    }

    /// The file, or `304` when the client already has it.
    fn response(&self, request: &HeaderMap) -> Response {
        let headers = [
            (CONTENT_TYPE, HeaderValue::from_static(self.content_type)),
            (ETAG, self.etag.clone()),
            // Revalidated on every load: an upgraded server serves its own page at once.
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ];
        if request.get(IF_NONE_MATCH) == Some(&self.etag) {
            return (StatusCode::NOT_MODIFIED, headers).into_response();
        }
        (headers, self.bytes).into_response()
    }
}

/// The chat page of a server.
#[derive(Debug)]
pub struct Chat {
    workflows: Vec<ChatWorkflow>,
    access: ChatAccess,
    title: Option<String>,
    guests: Option<Guests>,
    has_api_key: bool,
    /// The header that carries a user token (`--verify-jwt`), with `--chat-access users`.
    user_token_header: Option<String>,
    page: Asset,
    page_script: Asset,
    widget_script: Asset,
    /// The page's `Content-Security-Policy`.
    page_policy: HeaderValue,
}

/// What [`Chat::new`] needs to know about the rest of the server.
#[derive(Debug, Default)]
pub struct ChatContext<'a> {
    pub api_key: Option<&'a str>,
    /// The header verified by `--verify-jwt`, when it is on.
    pub user_token_header: Option<String>,
    /// Origins that may embed the page in a frame (the `--cors-origin` list).
    pub frame_origins: Vec<String>,
}

impl Chat {
    /// The chat page for `args`, or `None` without `--chat`.
    ///
    /// @param start_inputs - The declared start inputs of a served workflow, `None` for one not served.
    /// @throws CliError (usage) - A workflow that is not served, an input it does not declare,
    ///   `--chat-access users` without `--verify-jwt`, or `guests` behind user tokens.
    pub fn new(
        args: &ChatArgs,
        context: ChatContext<'_>,
        start_inputs: impl Fn(&str) -> Option<Vec<String>>,
    ) -> Result<Option<Self>, CliError> {
        if args.chat.is_empty() {
            return Ok(None);
        }
        let mut workflows: Vec<ChatWorkflow> = Vec::new();
        for spec in &args.chat {
            let workflow = ChatWorkflow::parse(spec).map_err(CliError::usage)?;
            let Some(inputs) = start_inputs(&workflow.name) else {
                return Err(CliError::usage(format!("--chat `{}`: this workflow is not served", workflow.name)));
            };
            if let Some(input) = &workflow.input
                && !inputs.contains(input)
            {
                return Err(CliError::usage(format!(
                    "--chat `{spec}`: workflow `{}` declares no start input `{input}`",
                    workflow.name
                )));
            }
            if !workflows.iter().any(|known| known.name == workflow.name) {
                workflows.push(workflow);
            }
        }
        let guests = match args.chat_access {
            ChatAccess::Guests => {
                Some(Guests::new(context.api_key, args.chat_guest_rate_limit, args.chat_guest_budget)?)
            }
            ChatAccess::Users if context.user_token_header.is_none() => {
                return Err(CliError::usage(
                    "--chat-access users needs --verify-jwt: the chat page sends the user's token for the server to verify",
                ));
            }
            ChatAccess::Key | ChatAccess::Users => None,
        };
        let page_policy = page_policy(&context.frame_origins)?;
        Ok(Some(Self {
            workflows,
            access: args.chat_access,
            title: args.chat_title.clone().filter(|title| !title.trim().is_empty()),
            guests,
            has_api_key: context.api_key.is_some(),
            user_token_header: context.user_token_header.filter(|_| args.chat_access == ChatAccess::Users),
            page: Asset::new(PAGE, "text/html; charset=utf-8"),
            page_script: Asset::new(PAGE_SCRIPT, "text/javascript; charset=utf-8"),
            widget_script: Asset::new(WIDGET_SCRIPT, "text/javascript; charset=utf-8"),
            page_policy,
        }))
    }

    /// Whether the page offers workflow `name`.
    pub(crate) fn offers(&self, name: &str) -> bool {
        self.workflows.iter().any(|workflow| workflow.name == name)
    }

    /// The guest sessions, with `--chat-access guests`.
    pub(crate) fn guests(&self) -> Option<&Guests> {
        self.guests.as_ref()
    }

    /// Whether anyone may chat without signing in.
    pub fn is_public(&self) -> bool {
        self.guests.is_some()
    }

    /// Lines for the startup log.
    pub fn describe(&self) -> Vec<String> {
        let who = match self.access {
            ChatAccess::Key => "holders of the API key",
            ChatAccess::Users => "verified end users",
            ChatAccess::Guests => "anyone (guest sessions)",
        };
        let mut lines: Vec<String> =
            self.workflows.iter().map(|workflow| format!("GET  /chat/{} (chat page)", workflow.name)).collect();
        lines.push(format!("chat page and widget (/chat/embed.js) for {who}"));
        lines
    }

    /// What `GET /v1/chat/config` answers.
    fn config(&self, state: &AppState) -> serde_json::Value {
        let access = match self.access {
            // Without an API key nothing needs to be entered.
            ChatAccess::Key if !self.has_api_key => "open",
            ChatAccess::Key => "key",
            ChatAccess::Users => "users",
            ChatAccess::Guests => "guests",
        };
        let workflows: Vec<serde_json::Value> = self
            .workflows
            .iter()
            .map(|workflow| serde_json::json!({ "name": workflow.name, "input": workflow.input }))
            .collect();
        let title = self.title.clone().or_else(|| self.workflows.first().map(|workflow| workflow.name.clone()));
        serde_json::json!({
            "title": title.unwrap_or_default(),
            "workflows": workflows,
            "access": access,
            "user_token_header": self.user_token_header,
            "conversations": state.keeps_conversations(),
            "resume": state.user_resume || matches!(self.access, ChatAccess::Key),
            "feedback": cfg!(feature = "scores"),
        })
    }
}

/// The policy of the page: scripts, styles and connections of its own origin
/// only, images also from `https` and `data:` (what UI blocks may show), and
/// framing by its own origin and the origins of `--cors-origin`.
///
/// @throws CliError (usage) - When an origin cannot be part of a header.
fn page_policy(frame_origins: &[String]) -> Result<HeaderValue, CliError> {
    let mut ancestors = String::from("'self'");
    for origin in frame_origins {
        ancestors.push(' ');
        ancestors.push_str(origin);
    }
    let policy = format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https:; \
         connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors {ancestors}"
    );
    HeaderValue::from_str(&policy).map_err(|_| CliError::usage("--cors-origin: an origin is not a valid header value"))
}

/// The chat routes. They authenticate themselves: the page and its config are
/// public, everything else the page calls is the ordinary API.
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/chat", get(redirect_to_page))
        .route("/chat/", get(page))
        .route("/chat/chat.js", get(page_script))
        .route("/chat/embed.js", get(widget_script))
        .route("/chat/{workflow}", get(workflow_page))
        .route("/v1/chat/config", get(config))
        .route("/v1/chat/guest", post(guest_session))
}

/// The chat of a server that has one. The routes exist only then.
fn chat_of(state: &AppState) -> Result<&Chat, Response> {
    state.chat.as_deref().ok_or_else(|| error_response(StatusCode::NOT_FOUND, "this server has no chat page"))
}

/// `GET /chat`: to `/chat/`, relative so it also works below a path prefix of a proxy.
async fn redirect_to_page() -> Response {
    (StatusCode::PERMANENT_REDIRECT, [(LOCATION, HeaderValue::from_static("chat/"))]).into_response()
}

async fn page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match chat_of(&state) {
        Ok(chat) => html(chat, &headers),
        Err(refused) => refused,
    }
}

async fn workflow_page(
    State(state): State<Arc<AppState>>,
    UrlPath(workflow): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    match chat_of(&state) {
        Ok(chat) if chat.offers(&workflow) => html(chat, &headers),
        Ok(_) => error_response(StatusCode::NOT_FOUND, "no chat page for this workflow"),
        Err(refused) => refused,
    }
}

fn html(chat: &Chat, request: &HeaderMap) -> Response {
    let mut response = chat.page.response(request);
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, chat.page_policy.clone());
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

async fn page_script(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match chat_of(&state) {
        Ok(chat) => chat.page_script.response(&headers),
        Err(refused) => refused,
    }
}

async fn widget_script(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match chat_of(&state) {
        Ok(chat) => chat.widget_script.response(&headers),
        Err(refused) => refused,
    }
}

/// `GET /v1/chat/config`.
async fn config(State(state): State<Arc<AppState>>) -> Response {
    match chat_of(&state) {
        Ok(chat) => Json(chat.config(&state)).into_response(),
        Err(refused) => refused,
    }
}

/// `POST /v1/chat/guest`: a new guest session, or a renewed one when the
/// request carries the guest's earlier token.
async fn guest_session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(guests) = state.chat.as_deref().and_then(Chat::guests) else {
        return error_response(StatusCode::NOT_FOUND, "this server has no guest chat");
    };
    if state.is_draining() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "the server is shutting down; retry shortly");
    }
    let previous = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| is_guest_token(token));
    // A token that cannot be renewed (too old, another key) starts a new guest.
    let issued = match previous.map(|token| guests.renew(token)) {
        Some(Ok(issued)) => Ok(issued),
        Some(Err(_)) | None => guests.issue(),
    };
    match issued {
        Ok(issued) => {
            let body = serde_json::json!({ "token": issued.token, "expires_ms": issued.expires_ms });
            (StatusCode::CREATED, [(CACHE_CONTROL, HeaderValue::from_static("no-store"))], Json(body)).into_response()
        }
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot open a guest session"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(chat: &[&str], access: ChatAccess) -> ChatArgs {
        ChatArgs {
            chat: chat.iter().map(|spec| (*spec).to_owned()).collect(),
            chat_access: access,
            chat_title: None,
            chat_guest_rate_limit: 10,
            chat_guest_budget: 120,
        }
    }

    fn inputs(name: &str) -> Option<Vec<String>> {
        (name == "support").then(|| vec!["message".to_owned()])
    }

    #[test]
    fn no_chat_flag_means_no_chat() {
        assert!(Chat::new(&args(&[], ChatAccess::Key), ChatContext::default(), inputs).unwrap().is_none());
    }

    #[test]
    fn workflows_and_their_inputs_must_exist() {
        let chat = Chat::new(&args(&["support=message", "support"], ChatAccess::Key), ChatContext::default(), inputs)
            .unwrap()
            .unwrap();
        assert_eq!(chat.workflows, [ChatWorkflow { name: "support".into(), input: Some("message".into()) }]);
        assert!(chat.offers("support") && !chat.offers("internal"));
        for bad in ["internal", "support=nope", "=message", "support="] {
            assert!(Chat::new(&args(&[bad], ChatAccess::Key), ChatContext::default(), inputs).is_err(), "{bad}");
        }
    }

    #[test]
    fn user_access_needs_token_verification_and_guests_get_sessions() {
        assert!(Chat::new(&args(&["support"], ChatAccess::Users), ChatContext::default(), inputs).is_err());
        let context = ChatContext { user_token_header: Some("x-user-authorization".into()), ..Default::default() };
        let users = Chat::new(&args(&["support"], ChatAccess::Users), context, inputs).unwrap().unwrap();
        assert!(users.guests().is_none() && !users.is_public());
        let guests =
            Chat::new(&args(&["support"], ChatAccess::Guests), ChatContext::default(), inputs).unwrap().unwrap();
        assert!(guests.is_public());
    }

    #[test]
    fn the_page_policy_allows_only_its_own_origin_and_listed_frames() {
        let policy = page_policy(&["https://app.example.com".to_owned()]).unwrap();
        let policy = policy.to_str().unwrap();
        assert!(policy.contains("script-src 'self';"));
        assert!(policy.contains("connect-src 'self';"));
        assert!(policy.ends_with("frame-ancestors 'self' https://app.example.com"));
        assert!(!policy.contains("unsafe"));
    }

    #[test]
    fn an_asset_is_not_sent_again_to_a_client_that_has_it() {
        let asset = Asset::new(b"console.log(1)", "text/javascript; charset=utf-8");
        let first = asset.response(&HeaderMap::new());
        assert_eq!(first.status(), StatusCode::OK);
        let mut request = HeaderMap::new();
        request.insert(IF_NONE_MATCH, first.headers().get(ETAG).unwrap().clone());
        assert_eq!(asset.response(&request).status(), StatusCode::NOT_MODIFIED);
    }
}
