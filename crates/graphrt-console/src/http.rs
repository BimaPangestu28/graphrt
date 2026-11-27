//! The HTTP side of the console: the gate in front of every route of the
//! server, the sign in pages and the console API.
//!
//! | Method | Path | Role | What |
//! | --- | --- | --- | --- |
//! | GET | `/console/login` | – | start a sign in (`?return_to=/console/`) |
//! | GET | `/console/callback` | – | where the provider sends the browser back |
//! | GET | `/v1/console/session` | – | who is signed in, their role and CSRF token |
//! | POST | `/v1/console/sign-out` | viewer | end this session |
//! | GET | `/v1/console/workflows` | viewer | served workflows, registry versions, environments, pending changes |
//! | POST | `/v1/console/workflows/{name}/versions` | owner | upload a version (the file as the body; `?environment=`, `?note=`, `?format=`) |
//! | GET | `/v1/console/workflows/{name}/versions/{version}/file` | owner | download a version |
//! | POST | `/v1/console/workflows/{name}/deploy` | owner | `{version, environment}` |
//! | POST | `/v1/console/workflows/{name}/promote` | owner | `{from, to?}` |
//! | POST | `/v1/console/workflows/{name}/rollback` | owner | `{environment, version?}` |
//! | GET | `/v1/console/secrets` | operator | secret names, never values |
//! | PUT, DELETE | `/v1/console/secrets/{name}` | owner | `{value}` |
//! | GET | `/v1/console/members` | owner | everyone who signed in, with their roles |
//! | PUT | `/v1/console/members/{subject}` | owner | `{role}`: `owner`, `operator`, `viewer`, `none`, or `null` for the mapped role |
//! | GET, DELETE | `/v1/console/sessions`, `/v1/console/sessions/{id}` | owner | live sessions; end one |
//! | GET | `/v1/console/audit` | owner | entries, newest first (`since_ms`, `until_ms`, `actor`, `action`, `target`, `outcome`, `before_seq`, `limit`) |
//! | GET | `/v1/console/audit/export` | owner | the same as JSON lines, oldest first |
//! | GET | `/v1/console/audit/verify` | owner | check the hash chain |
//! | POST | `/v1/console/apply` | owner | restart the server so changes take effect |
//!
//! The console API takes a signed in session (with `X-Graphrt-CSRF` on every
//! request that changes something) or the API key as a bearer token.

// Handlers answer a refusal as the `Response` it is; boxing it gains nothing
// on a path that ends the request.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path as UrlPath, Query, Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, LOCATION,
    ORIGIN, REFERRER_POLICY, SET_COOKIE,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::Deserialize;

use crate::audit::{Actor, Event};
use crate::error::ConsoleError;
use crate::registry::Format;
use crate::roles::{Assigned, Role};
use crate::sessions::{PendingLogin, Session};
use crate::util::{random_token, sha256_hex};
use crate::{Console, policy};

/// The header that carries the session's CSRF token.
pub const CSRF_HEADER: &str = "x-graphrt-csrf";
/// The prefix of device tokens of `graphrt serve --devices`.
const DEVICE_TOKEN_PREFIX: &str = "grd1.";
/// The policy of the console's own HTML (the sign in messages and the page).
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Who makes a request that the gate let through.
#[derive(Clone, Debug)]
pub enum Principal {
    /// A person signed in with the identity provider.
    User { session: Box<Session>, role: Role },
    /// The holder of the API key: an owner.
    ApiKey,
}

impl Principal {
    /// What the person may do.
    pub fn role(&self) -> Role {
        match self {
            Principal::User { role, .. } => *role,
            Principal::ApiKey => Role::Owner,
        }
    }

    /// Who it is, for the audit log.
    pub fn actor(&self) -> Actor {
        match self {
            Principal::User { session, role } => Actor {
                kind: "user".into(),
                id: session.identity.subject.clone(),
                email: session.identity.email.clone(),
                role: Some(role.name().into()),
            },
            Principal::ApiKey => Actor::api_key(),
        }
    }

    /// A short name of who it is, for "updated by".
    pub fn label(&self) -> String {
        match self {
            Principal::User { session, .. } => {
                session.identity.email.clone().unwrap_or_else(|| session.identity.subject.clone())
            }
            Principal::ApiKey => "api-key".into(),
        }
    }

    fn require(&self, role: Role) -> Result<(), Response> {
        if self.role() >= role {
            Ok(())
        } else {
            Err(json_error(StatusCode::FORBIDDEN, &format!("this needs the {} role", role.name())))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "sign in, or send the API key"))
    }
}

/// The [`Principal`] of a request on a public route, when there is one.
#[derive(Debug)]
pub struct MaybePrincipal(pub Option<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for MaybePrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<Principal>().cloned()))
    }
}

impl IntoResponse for ConsoleError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() && !matches!(self, ConsoleError::SignIn(_)) {
            // I/O and key errors name server paths: in the log, not in the answer.
            eprintln!("console error: {self}");
            return json_error(status, "console error (details in the server log)");
        }
        json_error(status, &self.to_string())
    }
}

fn json_error(status: StatusCode, message: &str) -> Response {
    no_store((status, Json(serde_json::json!({ "error": message }))).into_response())
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn ok_json(value: serde_json::Value) -> Response {
    no_store(Json(value).into_response())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
}

/// The value of cookie `name`.
fn cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

fn is_unsafe(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

impl Console {
    fn session_cookie(&self) -> &'static str {
        if self.public().https { "__Host-graphrt-session" } else { "graphrt-session" }
    }

    fn login_cookie(&self) -> &'static str {
        if self.public().https { "__Host-graphrt-login" } else { "graphrt-login" }
    }

    fn set_cookie(&self, name: &str, value: &str, max_age_secs: u64, same_site: &str) -> HeaderValue {
        let secure = if self.public().https { "; Secure" } else { "" };
        let text = format!("{name}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite={same_site}{secure}");
        HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("graphrt-invalid=; Max-Age=0"))
    }

    /// DNS rebinding protection: a browser request must name this server's host.
    fn host_allowed(&self, headers: &HeaderMap) -> bool {
        let host = headers.get(HOST).and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase);
        host.is_some_and(|host| self.public().hosts.contains(&host))
    }

    /// A request that changes something must come from the console's own pages.
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let origin_ok = headers
            .get(ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|origin| origin.eq_ignore_ascii_case(&self.public().origin));
        let site_ok = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|site| site == "same-origin" || site == "none");
        origin_ok && site_ok
    }

    fn is_api_key(&self, token: &str) -> bool {
        crate::util::constant_time_eq(token.as_bytes(), self.api_key().as_bytes())
    }
}

fn is_console_route(path: &str) -> bool {
    path == "/console" || path.starts_with("/console/") || path.starts_with("/v1/console/")
}

/// The gate in front of every route of the server.
///
/// - A request with a bearer token keeps the server's own rules (the API
///   key, a device token, an end user token); the console API takes the API
///   key only. Owner actions, approvals and device changes are audited.
/// - A request with a session cookie must name an allowed `Host`, come from
///   the console's origin with the session's CSRF token when it changes
///   something, and have the role [`policy::rule`] asks for. It then goes on
///   as the API key would, and what it changed is audited.
/// - Any other request goes on untouched: the server's own checks answer it.
pub async fn gate(State(console): State<Arc<Console>>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    request.extensions_mut().remove::<Principal>();
    let rule = policy::rule(&method, &path);
    let console_route = is_console_route(&path);
    if let Some(token) = bearer(request.headers()) {
        let is_key = console.is_api_key(token);
        if console_route {
            if is_key {
                request.extensions_mut().insert(Principal::ApiKey);
            } else if rule.role.is_some() {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "the console API takes a signed in session or the API key",
                );
            }
            return next.run(request).await;
        }
        let actor = if is_key {
            Some(Actor::api_key())
        } else if let Some(rest) = token.strip_prefix(DEVICE_TOKEN_PREFIX) {
            let id = rest.split('.').next().unwrap_or_default().to_owned();
            Some(Actor { kind: "device".into(), id, email: None, role: None })
        } else {
            None
        };
        let response = next.run(request).await;
        if let (Some(action), Some(actor), true) = (rule.action, actor, rule.bearer) {
            audit_request(&console, actor, action, &method, &path, response.status());
        }
        return response;
    }
    if rule.role.is_none() {
        // A public page or sign in: a session changes nothing, except that
        // the console's own public routes see who is signed in.
        if console_route
            && let Some(session) = signed_in(&console, request.headers())
            && let Ok(Some(role)) = console.members.role_of(&session.identity.subject)
        {
            request.extensions_mut().insert(Principal::User { session: Box::new(session), role });
        }
        return next.run(request).await;
    }
    let Some(session) = signed_in(&console, request.headers()) else {
        if console_route {
            return json_error(StatusCode::UNAUTHORIZED, "sign in, or send the API key");
        }
        return next.run(request).await;
    };
    if !console.host_allowed(request.headers()) {
        return json_error(StatusCode::FORBIDDEN, "this host name is not the console's (see allowed_hosts)");
    }
    if is_unsafe(&method) {
        let csrf = request.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !console.same_origin(request.headers()) || !session.csrf_matches(csrf) {
            return json_error(StatusCode::FORBIDDEN, "missing or wrong CSRF token, or a request from another site");
        }
    }
    let role = match console.members.role_of(&session.identity.subject) {
        Ok(Some(role)) => role,
        Ok(None) => {
            let _ended = console.sessions.end(&session.id);
            return json_error(StatusCode::UNAUTHORIZED, "your access to this console was removed");
        }
        Err(error) => return error.into_response(),
    };
    let principal = Principal::User { session: Box::new(session), role };
    if !rule.sessions {
        return json_error(
            StatusCode::FORBIDDEN,
            "this route is for the systems that call it with their own credentials",
        );
    }
    if let Some(required) = rule.role
        && role < required
    {
        let action = rule.action.unwrap_or("api.read");
        audit_request(&console, principal.actor(), action, &method, &path, StatusCode::FORBIDDEN);
        return json_error(StatusCode::FORBIDDEN, &format!("this needs the {} role", required.name()));
    }
    request.extensions_mut().insert(principal.clone());
    if !console_route {
        // Let it through as the API key would be: the routes behind the gate
        // know no sessions. The key is set here, never sent to the browser.
        match HeaderValue::from_str(&format!("Bearer {}", console.api_key())) {
            Ok(value) => {
                request.headers_mut().insert(AUTHORIZATION, value);
            }
            Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "the API key is not a header value"),
        }
        request.headers_mut().remove(COOKIE);
    }
    let response = next.run(request).await;
    if !console_route && is_unsafe(&method) {
        let action = rule.action.unwrap_or("api.change");
        audit_request(&console, principal.actor(), action, &method, &path, response.status());
    }
    response
}

fn signed_in(console: &Console, headers: &HeaderMap) -> Option<Session> {
    let token = cookie(headers, console.session_cookie())?;
    match console.sessions.lookup(token) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("warning: console: cannot read the sessions: {error}");
            None
        }
    }
}

fn audit_request(console: &Console, actor: Actor, action: &str, method: &Method, path: &str, status: StatusCode) {
    let mut event = Event::ok(actor, action)
        .status(status.as_u16())
        .details(serde_json::json!({ "method": method.as_str(), "path": path }));
    if let Some(target) = policy::target(method, path) {
        event = event.target(target);
    }
    console.record(event);
}

/// The console's routes, to merge into the server's router. Uploads of
/// versions may be up to `max_upload_bytes`.
pub fn routes<S: Clone + Send + Sync + 'static>(console: &Arc<Console>, max_upload_bytes: usize) -> Router<S> {
    Router::new()
        .route("/console", get(redirect_to_page))
        .route("/console/", get(page))
        .route("/console/login", get(login))
        .route("/console/callback", get(callback))
        .route("/v1/console/session", get(session))
        .route("/v1/console/sign-out", post(sign_out))
        .route("/v1/console/workflows", get(workflows))
        .route("/v1/console/workflows/{name}/versions", post(upload).layer(DefaultBodyLimit::max(max_upload_bytes)))
        .route("/v1/console/workflows/{name}/versions/{version}/file", get(download))
        .route("/v1/console/workflows/{name}/deploy", post(deploy))
        .route("/v1/console/workflows/{name}/promote", post(promote))
        .route("/v1/console/workflows/{name}/rollback", post(rollback))
        .route("/v1/console/secrets", get(list_secrets))
        .route("/v1/console/secrets/{name}", put(set_secret).delete(delete_secret))
        .route("/v1/console/members", get(list_members))
        .route("/v1/console/members/{subject}", put(assign_role))
        .route("/v1/console/sessions", get(list_sessions))
        .route("/v1/console/sessions/{id}", axum::routing::delete(end_session))
        .route("/v1/console/audit", get(audit_entries))
        .route("/v1/console/audit/export", get(audit_export))
        .route("/v1/console/audit/verify", get(audit_verify))
        .route("/v1/console/apply", post(apply))
        .with_state(Arc::clone(console))
}

// ---- pages and sign in ---------------------------------------------------------------

/// The console page the host serves at `/console/`, when it has one.
#[derive(Debug, Clone)]
pub struct Page {
    pub html: &'static [u8],
}

const DEFAULT_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Graphrt console</title></head>\
<body><main><h1>Graphrt console</h1><p><a href=\"login\">Sign in</a></p>\
<p>The console API is under <code>/v1/console/</code>; see docs/console.md.</p></main></body></html>";

async fn redirect_to_page(State(console): State<Arc<Console>>) -> Response {
    let location = format!("{}/console/", console.public().prefix);
    (StatusCode::PERMANENT_REDIRECT, [(LOCATION, location)]).into_response()
}

async fn page() -> Response {
    html(StatusCode::OK, DEFAULT_PAGE.to_owned())
}

fn html(status: StatusCode, body: String) -> Response {
    let mut response = no_store((status, body).into_response());
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_POLICY));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

/// A short page that says what happened, with a way to try again.
fn message_page(console: &Console, status: StatusCode, title: &str, message: &str) -> Response {
    let again = format!("{}/console/login", console.public().prefix);
    html(
        status,
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title></head>\
             <body><main><h1>{}</h1><p>{}</p><p><a href=\"{}\">Sign in again</a></p></main></body></html>",
            escape(title),
            escape(title),
            escape(message),
            escape(&again)
        ),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A path of this server to return to after the sign in; anything else is the console page.
fn safe_return(return_to: Option<&str>) -> String {
    match return_to {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.chars().any(char::is_control)
                && path.len() <= 512 =>
        {
            path.to_owned()
        }
        _ => "/console/".to_owned(),
    }
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    return_to: Option<String>,
}

async fn login(State(console): State<Arc<Console>>, headers: HeaderMap, Query(query): Query<LoginQuery>) -> Response {
    if !console.host_allowed(&headers) {
        return message_page(&console, StatusCode::FORBIDDEN, "Wrong address", "Open the console at its own address.");
    }
    let secrets = (random_token(), random_token(), random_token(), random_token());
    let (Ok(state), Ok(nonce), Ok(verifier), Ok(binding)) = secrets else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "no system randomness");
    };
    let url = match console.provider.authorization_url(&state, &nonce, &verifier).await {
        Ok(url) => url,
        Err(error) => {
            console.provider.reset().await;
            eprintln!("warning: console: {error}");
            return message_page(
                &console,
                StatusCode::BAD_GATEWAY,
                "Sign in is not available",
                "The identity provider cannot be reached right now. Try again in a moment.",
            );
        }
    };
    let return_to = safe_return(query.return_to.as_deref());
    let login = PendingLogin {
        binding_hash: sha256_hex(binding.as_bytes()),
        verifier,
        nonce,
        return_to,
        created_ms: crate::util::now_ms(),
    };
    console.pending.insert(state, login);
    let max_age = crate::sessions::LOGIN_TTL_MS / 1000;
    let cookie = console.set_cookie(console.login_cookie(), &binding, max_age, "Lax");
    let mut response = no_store((StatusCode::SEE_OTHER, [(LOCATION, url)]).into_response());
    response.headers_mut().insert(SET_COOKIE, cookie);
    response
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(console): State<Arc<Console>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if !console.host_allowed(&headers) {
        return message_page(&console, StatusCode::FORBIDDEN, "Wrong address", "Open the console at its own address.");
    }
    if let Some(error) = &query.error {
        let reason: String = error.chars().filter(|c| c.is_ascii_graphic()).take(64).collect();
        return message_page(
            &console,
            StatusCode::UNAUTHORIZED,
            "Sign in refused",
            &format!("The identity provider did not sign you in ({reason})."),
        );
    }
    let expired = || {
        message_page(&console, StatusCode::BAD_REQUEST, "Sign in expired", "This sign in expired or was used already.")
    };
    let (Some(code), Some(state)) = (query.code.as_deref(), query.state.as_deref()) else { return expired() };
    let Some(pending) = console.pending.take(state) else { return expired() };
    let binding = cookie(&headers, console.login_cookie()).unwrap_or_default();
    if !crate::util::constant_time_eq(sha256_hex(binding.as_bytes()).as_bytes(), pending.binding_hash.as_bytes()) {
        // Another browser than the one that started it: a forged sign in.
        return expired();
    }
    let claims = match console.provider.exchange(code, &pending.verifier, &pending.nonce).await {
        Ok(claims) => claims,
        Err(error) => {
            console.provider.reset().await;
            eprintln!("warning: console: {error}");
            return message_page(&console, StatusCode::BAD_GATEWAY, "Sign in failed", &error.to_string());
        }
    };
    let groups_claim = &console.config().oidc.groups_claim;
    let Some(identity) = crate::roles::Identity::from_claims(&claims, groups_claim) else {
        return message_page(&console, StatusCode::BAD_REQUEST, "Sign in failed", "The ID token names nobody.");
    };
    let actor = Actor { kind: "user".into(), id: identity.subject.clone(), email: identity.email.clone(), role: None };
    let role = match console.members.signed_in(&console.config().roles, &identity) {
        Ok(role) => role,
        Err(error) => return error.into_response(),
    };
    let Some(role) = role else {
        let details = serde_json::json!({ "groups": identity.groups });
        console.record(Event::ok(actor, "auth.sign_in").outcome("denied").details(details));
        return message_page(
            &console,
            StatusCode::FORBIDDEN,
            "No access",
            "Your account has no role in this console. Ask an owner for access.",
        );
    };
    let (token, session) = match console.sessions.create(identity) {
        Ok(created) => created,
        Err(error) => return error.into_response(),
    };
    let actor = Actor { role: Some(role.name().into()), ..actor };
    console.record(Event::ok(actor, "auth.sign_in").details(serde_json::json!({ "session": session.id })));
    let location = format!("{}{}", console.public().prefix, pending.return_to);
    let mut response = no_store((StatusCode::SEE_OTHER, [(LOCATION, location)]).into_response());
    let headers = response.headers_mut();
    let max_age = console.sessions.max_age_secs();
    headers.append(SET_COOKIE, console.set_cookie(console.session_cookie(), &token, max_age, "Strict"));
    headers.append(SET_COOKIE, console.set_cookie(console.login_cookie(), "", 0, "Lax"));
    response
}

async fn session(State(console): State<Arc<Console>>, MaybePrincipal(principal): MaybePrincipal) -> Response {
    let mut answer = serde_json::Map::new();
    answer.insert("environment".into(), console.environment().into());
    answer.insert("environments".into(), console.registry.environments().into());
    answer.insert("sign_in_url".into(), format!("{}/console/login", console.public().prefix).into());
    answer.insert("secrets_key".into(), console.secrets.has_key().into());
    answer.insert("signed_in".into(), principal.is_some().into());
    match principal {
        Some(Principal::User { session, role }) => {
            answer.insert("via".into(), "session".into());
            answer.insert("role".into(), role.name().into());
            answer.insert("csrf_token".into(), session.csrf.clone().into());
            let user = serde_json::json!({
                "subject": session.identity.subject,
                "email": session.identity.email,
                "name": session.identity.name,
            });
            answer.insert("user".into(), user);
            answer.insert("expires_ms".into(), session.expires_ms.into());
        }
        Some(Principal::ApiKey) => {
            answer.insert("via".into(), "api-key".into());
            answer.insert("role".into(), "owner".into());
        }
        None => {}
    }
    ok_json(serde_json::Value::Object(answer))
}

async fn sign_out(State(console): State<Arc<Console>>, principal: Principal) -> Response {
    let mut response = match &principal {
        Principal::User { session, .. } => {
            if let Err(error) = console.sessions.end(&session.id) {
                return error.into_response();
            }
            console.record(Event::ok(principal.actor(), "auth.sign_out"));
            let end_session_url = console.provider.end_session_url().await;
            ok_json(serde_json::json!({ "signed_out": true, "end_session_url": end_session_url }))
        }
        Principal::ApiKey => ok_json(serde_json::json!({ "signed_out": false })),
    };
    response.headers_mut().append(SET_COOKIE, console.set_cookie(console.session_cookie(), "", 0, "Strict"));
    response
}

// ---- registry ------------------------------------------------------------------------

async fn workflows(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Viewer)?;
    let served = console.host().map(|host| host.served()).unwrap_or_default();
    let registry = console.registry.list().map_err(IntoResponse::into_response)?;
    let pending = console.pending_changes().map_err(IntoResponse::into_response)?;
    Ok(ok_json(serde_json::json!({
        "environment": console.environment(),
        "environments": console.registry.environments(),
        "served": served,
        "registry": registry,
        "pending": pending,
    })))
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    environment: Option<String>,
    note: Option<String>,
    format: Option<String>,
}

async fn upload(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    crate::registry::check_workflow_name(&name).map_err(IntoResponse::into_response)?;
    if body.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "send the workflow file as the request body"));
    }
    let format = match query.format.as_deref() {
        Some(format) => Format::from_name(format)
            .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, "format is yaml, json or grt"))?,
        None => Format::sniff(&body),
    };
    let Some(host) = console.host().cloned() else {
        return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "the server is still starting"));
    };
    let warnings = host.validate(&name, format, &body).await.map_err(|why| {
        json_error(StatusCode::UNPROCESSABLE_ENTITY, &format!("this version does not compile: {why}"))
    })?;
    let first = console.registry.environments().first().cloned().unwrap_or_default();
    let environment = match query.environment.as_deref() {
        Some("") | Some("none") => None,
        Some(environment) => Some(environment.to_owned()),
        None => Some(first),
    };
    let note = query.note.unwrap_or_default();
    let (version, new) = console
        .registry
        .add(&name, format, &body, &note, &principal.label(), environment.as_deref())
        .map_err(IntoResponse::into_response)?;
    let details = serde_json::json!({
        "version": version.version, "hash": version.hash, "format": version.format,
        "size": version.size, "new": new, "environment": environment, "note": version.note,
    });
    console.record(Event::ok(principal.actor(), "registry.upload").target(name.clone()).details(details));
    let status = if new { StatusCode::CREATED } else { StatusCode::OK };
    let restart = environment.as_deref() == Some(console.environment());
    let answer = serde_json::json!({
        "version": version, "new": new, "environment": environment,
        "warnings": warnings, "restart_needed": restart,
    });
    Ok(no_store((status, Json(answer)).into_response()))
}

async fn download(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath((name, number)): UrlPath<(String, u32)>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let views = console.registry.list().map_err(IntoResponse::into_response)?;
    let version = views
        .iter()
        .find(|view| view.name == name)
        .and_then(|view| view.versions.iter().find(|v| v.version == number))
        .cloned()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "unknown version"))?;
    let path = console.registry.blob_path(&version);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| ConsoleError::Io(format!("cannot read {}: {error}", path.display())).into_response())?;
    let content_type = match version.format {
        Format::Yaml => "application/yaml",
        Format::Json => "application/json",
        Format::Bundle => "application/zip",
    };
    let file_name = format!("{name}-v{}.{}", version.version, version.format.extension());
    let disposition = format!("attachment; filename=\"{file_name}\"");
    Ok(no_store(([(CONTENT_TYPE, content_type.to_owned()), (CONTENT_DISPOSITION, disposition)], bytes).into_response()))
}

#[derive(Debug, Deserialize)]
struct DeployBody {
    version: u32,
    environment: String,
}

async fn deploy(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<DeployBody>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let version = console
        .registry
        .deploy(&name, body.version, &body.environment, &principal.label())
        .map_err(IntoResponse::into_response)?;
    let details =
        serde_json::json!({ "version": version.version, "hash": version.hash, "environment": body.environment });
    console.record(Event::ok(principal.actor(), "registry.deploy").target(name).details(details));
    Ok(changed(&console, &body.environment, &version))
}

#[derive(Debug, Deserialize)]
struct PromoteBody {
    from: String,
    to: Option<String>,
}

async fn promote(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<PromoteBody>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let (version, to) = console
        .registry
        .promote(&name, &body.from, body.to.as_deref(), &principal.label())
        .map_err(IntoResponse::into_response)?;
    let details = serde_json::json!({ "version": version.version, "hash": version.hash, "from": body.from, "to": to });
    console.record(Event::ok(principal.actor(), "registry.promote").target(name).details(details));
    Ok(changed(&console, &to, &version))
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    environment: String,
    version: Option<u32>,
}

async fn rollback(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RollbackBody>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let version = console
        .registry
        .rollback(&name, &body.environment, body.version, &principal.label())
        .map_err(IntoResponse::into_response)?;
    let details =
        serde_json::json!({ "version": version.version, "hash": version.hash, "environment": body.environment });
    console.record(Event::ok(principal.actor(), "registry.rollback").target(name).details(details));
    Ok(changed(&console, &body.environment, &version))
}

fn changed(console: &Console, environment: &str, version: &crate::registry::Version) -> Response {
    ok_json(serde_json::json!({
        "version": version,
        "environment": environment,
        "restart_needed": environment == console.environment(),
    }))
}

// ---- secrets -------------------------------------------------------------------------

async fn list_secrets(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Operator)?;
    let secrets = console.secrets.list().map_err(IntoResponse::into_response)?;
    let served = console.host().map(|host| host.served()).unwrap_or_default();
    let used_by = |name: &str| -> Vec<&str> {
        served.iter().filter(|w| w.env.iter().any(|e| e == name)).map(|w| w.name.as_str()).collect()
    };
    let listed: Vec<serde_json::Value> = secrets
        .iter()
        .map(|secret| {
            serde_json::json!({
                "name": secret.name, "updated_ms": secret.updated_ms,
                "updated_by": secret.updated_by, "used_by": used_by(&secret.name),
            })
        })
        .collect();
    let mut declared: Vec<&str> = served.iter().flat_map(|w| w.env.iter().map(String::as_str)).collect();
    declared.sort_unstable();
    declared.dedup();
    let declared: Vec<serde_json::Value> = declared
        .into_iter()
        .map(|name| {
            let set = secrets.iter().any(|s| s.name == name);
            serde_json::json!({ "name": name, "workflows": used_by(name), "in_console": set })
        })
        .collect();
    Ok(ok_json(serde_json::json!({
        "key_configured": console.secrets.has_key(),
        "secrets": listed,
        "declared": declared,
    })))
}

#[derive(Debug, Deserialize)]
struct SecretBody {
    value: String,
}

async fn set_secret(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SecretBody>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let existed = console.secrets.set(&name, &body.value, &principal.label()).map_err(IntoResponse::into_response)?;
    console.note_secret_change();
    let event = Event::ok(principal.actor(), "secret.set").target(name.clone());
    console.record(event.details(serde_json::json!({ "created": !existed })));
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    let answer = serde_json::json!({ "name": name, "created": !existed, "restart_needed": true });
    Ok(no_store((status, Json(answer)).into_response()))
}

async fn delete_secret(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    if !console.secrets.remove(&name).map_err(IntoResponse::into_response)? {
        return Err(json_error(StatusCode::NOT_FOUND, &format!("no secret `{name}`")));
    }
    console.note_secret_change();
    console.record(Event::ok(principal.actor(), "secret.delete").target(name));
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

// ---- people --------------------------------------------------------------------------

async fn list_members(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let members = console.members.list().map_err(IntoResponse::into_response)?;
    let roles = &console.config().roles;
    Ok(ok_json(serde_json::json!({
        "members": members.iter().map(crate::roles::Member::json).collect::<Vec<_>>(),
        "mapping": {
            "groups_claim": console.config().oidc.groups_claim,
            "groups": roles.groups,
            "default_role": roles.default_role,
            "allowed_domains": roles.allowed_domains,
        },
    })))
}

#[derive(Debug, Deserialize)]
struct RoleBody {
    role: Option<Assigned>,
}

async fn assign_role(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(subject): UrlPath<String>,
    Json(body): Json<RoleBody>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let before = console.members.get(&subject).map_err(IntoResponse::into_response)?.and_then(|m| m.role());
    let member = console.members.assign(&subject, body.role).map_err(IntoResponse::into_response)?;
    if member.role().is_none() {
        console.sessions.end_all_of(&subject).map_err(IntoResponse::into_response)?;
    }
    let details = serde_json::json!({
        "email": member.email, "from": before, "to": member.role(), "assigned": body.role,
    });
    console.record(Event::ok(principal.actor(), "member.role").target(subject).details(details));
    Ok(ok_json(member.json()))
}

async fn list_sessions(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let sessions = console.sessions.list().map_err(IntoResponse::into_response)?;
    Ok(ok_json(serde_json::json!({ "sessions": sessions.iter().map(Session::json).collect::<Vec<_>>() })))
}

async fn end_session(
    State(console): State<Arc<Console>>,
    principal: Principal,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    if !console.sessions.end(&id).map_err(IntoResponse::into_response)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such session"));
    }
    console.record(Event::ok(principal.actor(), "auth.session_end").target(id));
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

// ---- audit ---------------------------------------------------------------------------

async fn audit_entries(
    State(console): State<Arc<Console>>,
    principal: Principal,
    Query(query): Query<crate::audit::Query>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let page = console.audit.query(&query).map_err(IntoResponse::into_response)?;
    Ok(ok_json(serde_json::to_value(page).unwrap_or_default()))
}

async fn audit_export(
    State(console): State<Arc<Console>>,
    principal: Principal,
    Query(query): Query<crate::audit::Query>,
) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let days = console.audit.export_days(&query).map_err(IntoResponse::into_response)?;
    console.record(Event::ok(principal.actor(), "audit.export").details(serde_json::json!({
        "since_ms": query.since_ms, "until_ms": query.until_ms, "action": query.action,
    })));
    let reader = Arc::clone(&console);
    let stream = futures::stream::unfold(days.into_iter(), move |mut days| {
        let console = Arc::clone(&reader);
        let query = query.clone();
        async move {
            let path = days.next()?;
            let read = tokio::task::spawn_blocking(move || console.audit.export_day(&path, &query)).await;
            let chunk = match read {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(error)) => Err(std::io::Error::other(error.to_string())),
                Err(error) => Err(std::io::Error::other(error.to_string())),
            };
            Some((chunk, days))
        }
    });
    let headers = [
        (CONTENT_TYPE, "application/x-ndjson".to_owned()),
        (CONTENT_DISPOSITION, "attachment; filename=\"graphrt-audit.jsonl\"".to_owned()),
    ];
    Ok(no_store((headers, Body::from_stream(stream)).into_response()))
}

async fn audit_verify(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let verification = console.audit.verify().map_err(IntoResponse::into_response)?;
    Ok(ok_json(serde_json::to_value(verification).unwrap_or_default()))
}

// ---- apply ---------------------------------------------------------------------------

async fn apply(State(console): State<Arc<Console>>, principal: Principal) -> Result<Response, Response> {
    principal.require(Role::Owner)?;
    let Some(host) = console.host().cloned() else {
        return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "the server is still starting"));
    };
    let pending = console.pending_changes().map_err(IntoResponse::into_response)?;
    console.record(Event::ok(principal.actor(), "server.restart").details(serde_json::json!({ "pending": pending })));
    host.restart().map_err(|why| json_error(StatusCode::CONFLICT, &why))?;
    Ok(no_store(
        (StatusCode::ACCEPTED, Json(serde_json::json!({ "restarting": true, "pending": pending }))).into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_paths_stay_on_this_server() {
        assert_eq!(safe_return(Some("/monitor/#/approvals")), "/monitor/#/approvals");
        for bad in ["https://evil.example", "//evil.example", "/\\evil", "relative", "/a\nb"] {
            assert_eq!(safe_return(Some(bad)), "/console/", "{bad}");
        }
        assert_eq!(safe_return(None), "/console/");
    }

    #[test]
    fn cookies_are_found_by_name() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("a=1; graphrt-session=grs1.x"));
        headers.append(COOKIE, HeaderValue::from_static("b=2"));
        assert_eq!(cookie(&headers, "graphrt-session"), Some("grs1.x"));
        assert_eq!(cookie(&headers, "b"), Some("2"));
        assert_eq!(cookie(&headers, "session"), None);
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
