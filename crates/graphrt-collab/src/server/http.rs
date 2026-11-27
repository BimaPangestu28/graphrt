//! The HTTP API and the sync endpoint.
//!
//! Every route but the public ones (`/healthz`, `/readyz`, `/v1/info`,
//! `/join`, the sign in routes) needs `Authorization: Bearer <access token>`.
//! Workspace routes look up the caller's current role on every request: a
//! non member gets 404 (outsiders learn nothing), a member without the role
//! 403 (written to the audit log as `denied`).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use graphrt_console::Role;
use graphrt_console::audit::{Actor, Event};
use serde::Deserialize;
use serde_json::{Value, json};

use super::accounts::{Account, AccountKind, NewIdentity};
use super::error::{ServerError, ServerResult};
use super::room::{self, Outgoing};
use super::sync;
use super::tls::{PeerAddr, TlsListener, server_config};
use super::token::AccessClaims;
use super::util::{is_id, now_ms, random_token, sha256_hex};
use super::workspaces::{DEFAULT_INVITE_HOURS, Workspace, WorkspaceFiles};
use super::{AuthCode, Mode, PendingLogin, Server};
use crate::protocol::{BEARER_SUBPROTOCOL_PREFIX, Control, Frame, SUBPROTOCOL};

/// The largest request body.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// A failed request: JSON `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError(pub ServerError);

impl From<ServerError> for ApiError {
    fn from(error: ServerError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let message = match &self.0 {
            // Internal details go to the log, not to the caller.
            ServerError::Io(why) | ServerError::Internal(why) | ServerError::Config(why) => {
                eprintln!("warning: collab: {why}");
                "something went wrong on the server".to_owned()
            }
            other => other.to_string(),
        };
        let body = json!({ "error": { "code": self.0.code(), "message": message } });
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

type ApiResult = Result<Response, ApiError>;

/// Set `key` of a JSON object (a no op on other values).
fn set(value: &mut Value, key: &str, new: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_owned(), new);
    }
}

fn ok(value: Value) -> ApiResult {
    Ok(Json(value).into_response())
}

/// The caller of a request, from its access token.
#[derive(Debug)]
struct Caller {
    account: Account,
    claims: AccessClaims,
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
}

impl Server {
    fn caller(&self, headers: &HeaderMap) -> ServerResult<Caller> {
        let token = bearer(headers).ok_or_else(|| ServerError::Unauthorized("sign in first".into()))?;
        self.caller_of(token)
    }

    fn caller_of(&self, token: &str) -> ServerResult<Caller> {
        let claims = self.signing.verify(token, now_ms() / 1000)?;
        let account = self
            .accounts
            .get(&claims.sub)
            .filter(|a| !a.disabled)
            .ok_or_else(|| ServerError::Unauthorized("this account cannot sign in".into()))?;
        Ok(Caller { account, claims })
    }

    /// The workspace `ws` when the caller is a member with at least `min`.
    fn member(&self, caller: &Caller, ws: &str, min: Role, action: &str) -> ServerResult<Member> {
        if caller.claims.ws.as_deref().is_some_and(|scoped| scoped != ws) {
            return Err(ServerError::Forbidden("this access token is for another workspace".into()));
        }
        let (workspace, role) = self.workspaces.membership(ws, &caller.account.id)?;
        let files = self.workspaces.files(ws)?;
        if role < min {
            files.record(Event::ok(actor(&caller.account, Some(role)), action).outcome("denied").status(403));
            return Err(ServerError::Forbidden(format!("this needs the {} role in the workspace", min.name())));
        }
        Ok(Member { workspace, role, files })
    }
}

struct Member {
    workspace: Workspace,
    role: Role,
    files: Arc<WorkspaceFiles>,
}

fn actor(account: &Account, role: Option<Role>) -> Actor {
    Actor {
        kind: "user".into(),
        id: account.id.clone(),
        email: account.email.clone(),
        role: role.map(|r| r.name().into()),
    }
}

/// The router of the server. Mount it at the root. A path prefix of the
/// public URL is handled here.
pub fn router(server: &Arc<Server>) -> Router {
    let api = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ready" }))
        .route("/v1/info", get(info))
        .route("/join", get(join_page))
        .route("/v1/auth/login", get(login))
        .route("/v1/auth/callback", get(callback))
        .route("/v1/auth/token", post(token))
        .route("/v1/auth/scoped", post(scoped))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/invites/preview", post(preview_invite))
        .route("/v1/invites/redeem", post(redeem_invite))
        .route("/v1/me", get(me))
        .route("/v1/workspaces", get(list_workspaces).post(create_workspace))
        .route("/v1/workspaces/{ws}", get(get_workspace).patch(rename_workspace).delete(delete_workspace))
        .route("/v1/workspaces/{ws}/members", get(list_members))
        .route("/v1/workspaces/{ws}/members/{account}", put(set_member).delete(remove_member))
        .route("/v1/workspaces/{ws}/invites", get(list_invites).post(create_invite))
        .route("/v1/workspaces/{ws}/invites/{invite}", axum::routing::delete(revoke_invite))
        .route("/v1/workspaces/{ws}/projects", get(list_projects).post(create_project))
        .route("/v1/workspaces/{ws}/projects/{project}", get(get_project).patch(rename_project).delete(delete_project))
        .route("/v1/workspaces/{ws}/projects/{project}/sync", get(sync_session))
        .route("/v1/workspaces/{ws}/projects/{project}/versions", get(list_versions).post(save_version))
        .route("/v1/workspaces/{ws}/projects/{project}/versions/{version}", get(get_version))
        .route("/v1/workspaces/{ws}/projects/{project}/versions/{version}/restore", post(restore_version))
        .route("/v1/workspaces/{ws}/audit", get(audit))
        .route("/v1/workspaces/{ws}/secrets", get(list_secrets))
        .route("/v1/workspaces/{ws}/secrets/{name}", put(set_secret).delete(remove_secret))
        .route("/v1/workspaces/{ws}/services", get(list_services))
        .route("/v1/workspaces/{ws}/services/{plugin}/{service}", put(grant_service))
        .route("/v1/workspaces/{ws}/services/{plugin}/{service}/call", post(call_service))
        .layer(middleware::from_fn_with_state(Arc::clone(server), limit))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::clone(server));
    let prefix = server.public.prefix.clone();
    if prefix.is_empty() { api } else { Router::new().nest(&prefix, api) }
}

/// Serve on the configured address (HTTPS with `tls`) until `shutdown` resolves.
///
/// @throws ServerError - When the address cannot be bound or the TLS files are unusable.
pub async fn serve(
    server: Arc<Server>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> ServerResult<()> {
    let listen = server.config.listen;
    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| ServerError::Io(format!("cannot listen on {listen}: {error}")))?;
    serve_on(server, tcp, shutdown).await
}

/// Serve on `tcp` (tests bind a port first).
///
/// @throws ServerError - When the TLS files are unusable or the server fails.
pub async fn serve_on(
    server: Arc<Server>,
    tcp: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> ServerResult<()> {
    spawn_housekeeping(&server);
    let app = router(&server).into_make_service_with_connect_info::<PeerAddr>();
    let closing = Arc::clone(&server);
    // Open sync sessions are told to leave first, else the graceful shutdown waits for them.
    let shutdown = async move {
        shutdown.await;
        closing.rooms.checkpoint_all();
    };
    let result = match &server.config.tls {
        Some(tls) => {
            let listener = TlsListener::new(tcp, server_config(&tls.cert, &tls.key)?)?;
            axum::serve(listener, app).with_graceful_shutdown(shutdown).await
        }
        None => axum::serve(tcp, app).with_graceful_shutdown(shutdown).await,
    };
    server.shutdown();
    result.map_err(|error| ServerError::Io(error.to_string()))
}

fn spawn_housekeeping(server: &Arc<Server>) {
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut minute = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut ticks: u64 = 0;
        loop {
            minute.tick().await;
            let Some(server) = weak.upgrade() else { return };
            server.prune_logins();
            if ticks.is_multiple_of(24 * 60) {
                if let Err(error) = server.audit.prune() {
                    eprintln!("warning: collab: audit retention: {error}");
                }
                for (workspace, _) in server.workspaces.all() {
                    if let Ok(files) = server.workspaces.files(&workspace)
                        && let Err(error) = files.audit.prune()
                    {
                        eprintln!("warning: collab: audit retention of {workspace}: {error}");
                    }
                }
            }
            ticks += 1;
        }
    });
}

/// Rate limits: per account for signed in requests, per address otherwise.
async fn limit(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let key = match bearer(request.headers()).and_then(|t| server.signing.verify(t, now_ms() / 1000).ok()) {
        Some(claims) => format!("acct:{}", claims.sub),
        None => format!("ip:{}", client_ip(&server, &request)),
    };
    if !server.requests.allow(&key) {
        return ApiError(ServerError::TooMany("too many requests, slow down".into())).into_response();
    }
    next.run(request).await
}

fn client_ip(server: &Server, request: &Request) -> String {
    if server.config.behind_proxy
        && let Some(forwarded) = request.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(last) = forwarded.rsplit(',').next()
    {
        return last.trim().to_owned();
    }
    request
        .extensions()
        .get::<ConnectInfo<PeerAddr>>()
        .map(|ConnectInfo(PeerAddr(addr))| addr.ip().to_string())
        .unwrap_or_default()
}

async fn info(State(server): State<Arc<Server>>) -> ApiResult {
    let mode = match server.config.mode {
        Mode::Hosted => "hosted",
        Mode::SelfHosted => "self_hosted",
    };
    ok(json!({
        "server": "graphrt-collab",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": SUBPROTOCOL,
        "mode": mode,
        "public_url": server.public.join(""),
        "sign_in": {
            "oidc": !server.providers.is_empty(),
            "provider": server.providers.first().map(|p| p.issuer.clone()),
            "providers": server.providers.iter().map(|p| json!({ "id": p.id, "name": p.name })).collect::<Vec<_>>(),
            "invite_codes": server.config.auth.invite_accounts(),
        },
        "limits": {
            "max_message_bytes": server.config.limits.max_message_bytes,
            "max_doc_bytes": server.config.limits.max_doc_bytes,
        },
    }))
}

/// The page an invite link opens: how to join with the Desktop app. The code
/// stays in the URL fragment, which the browser never sends to a server.
async fn join_page(State(server): State<Arc<Server>>) -> Response {
    let server_url = html_escape(&server.public.join(""));
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
<title>Join a Graphrt team</title><style>body{{font:16px system-ui,sans-serif;max-width:36rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}code{{background:#eee;padding:.1rem .3rem}}</style></head>\
<body><h1>Join a Graphrt team</h1>\
<p>You were invited to a team workspace on <code>{server_url}</code>.</p>\
<ol><li>Open Graphrt Desktop.</li><li>Choose <b>Share</b>, then <b>Join a team</b>.</li><li>Paste this whole page address (the link you received).</li></ol>\
<p lang=\"id\">Buka Graphrt Desktop, pilih <b>Bagikan</b> lalu <b>Gabung tim</b>, dan tempel alamat halaman ini.</p>\
</body></html>"
    );
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The loopback address of a native app (RFC 8252 section 7.3).
fn is_loopback_redirect(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("http://") else { return false };
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => host_port,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]") && !rest.contains('@')
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    redirect_uri: String,
    state: String,
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    device: Option<String>,
    /// The id of the provider (`/v1/info` lists them). Default: the first one.
    #[serde(default)]
    provider: Option<String>,
}

/// Start a sign in with an identity provider, for an app listening on a loopback address.
async fn login(State(server): State<Arc<Server>>, Query(query): Query<LoginQuery>) -> ApiResult {
    if server.providers.is_empty() {
        return Err(
            ServerError::NotFound("this server has no identity provider: join with an invite code".into()).into()
        );
    }
    let index = match query.provider.as_deref() {
        None | Some("") => 0,
        Some(id) => server
            .providers
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| ServerError::NotFound(format!("this server has no identity provider `{id}`")))?,
    };
    let provider =
        &server.providers.get(index).ok_or_else(|| ServerError::NotFound("no such provider".into()))?.provider;
    if !is_loopback_redirect(&query.redirect_uri) {
        return Err(ServerError::BadRequest("redirect_uri must be an http loopback address of the app".into()).into());
    }
    if query.code_challenge_method.as_deref().unwrap_or("S256") != "S256"
        || !(43..=128).contains(&query.code_challenge.len())
    {
        return Err(ServerError::BadRequest("a PKCE S256 code_challenge is required".into()).into());
    }
    if query.state.is_empty() || query.state.len() > 256 {
        return Err(ServerError::BadRequest("state is 1 to 256 characters".into()).into());
    }
    let server_state = random_token()?;
    let nonce = random_token()?;
    let verifier = random_token()?;
    let url = provider.authorization_url(&server_state, &nonce, &verifier).await.map_err(ServerError::from)?;
    server.add_login(
        server_state,
        PendingLogin {
            provider: index,
            nonce,
            verifier,
            redirect_uri: query.redirect_uri,
            client_state: query.state,
            client_challenge: query.code_challenge,
            device: query.device.unwrap_or_default(),
            expires_ms: 0,
        },
    )?;
    Ok(Redirect::to(&url).into_response())
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// The identity provider sends the browser back here.
async fn callback(State(server): State<Arc<Server>>, Query(query): Query<CallbackQuery>) -> ApiResult {
    let state = query.state.unwrap_or_default();
    let pending = server.logins.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&state);
    let Some(pending) = pending.filter(|p| p.expires_ms > now_ms()) else {
        return Err(ServerError::BadRequest(
            "this sign in expired or was already used: start again from the app".into(),
        )
        .into());
    };
    let back = |params: &[(&str, &str)]| -> ApiResult {
        let mut url = pending.redirect_uri.clone();
        url.push(if url.contains('?') { '&' } else { '?' });
        let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", form_encode(v))).collect();
        url.push_str(&query.join("&"));
        Ok(Redirect::to(&url).into_response())
    };
    if let Some(error) = query.error {
        return back(&[("error", &error), ("state", &pending.client_state)]);
    }
    let Some(code) = query.code else {
        return back(&[("error", "invalid_request"), ("state", &pending.client_state)]);
    };
    let Some(signer) = server.providers.get(pending.provider) else {
        return back(&[("error", "server_error"), ("state", &pending.client_state)]);
    };
    let claims = match signer.provider.exchange(&code, &pending.verifier, &pending.nonce).await {
        Ok(claims) => claims,
        Err(error) => {
            signer.provider.reset().await;
            server.record(
                Event::ok(Actor::system(), "auth.sign_in")
                    .outcome("failed")
                    .details(json!({ "provider": signer.id, "why": error.to_string() })),
            );
            return back(&[
                ("error", "access_denied"),
                ("error_description", "the sign in could not be checked"),
                ("state", &pending.client_state),
            ]);
        }
    };
    let Some(mut identity) = graphrt_console::roles::Identity::from_claims(&claims, &signer.groups_claim) else {
        return back(&[("error", "access_denied"), ("state", &pending.client_state)]);
    };
    if let Some(claim) = &signer.email_verified_claim
        && !claim_is_true(claims.get(claim))
    {
        // The provider does not vouch for this email (e.g. an Entra tenant that set it freely).
        identity.email = None;
    }
    // Self hosted: the console's role rules say who may sign in at all.
    // Hosted: anyone, unless the rules narrow it (allowed domains, a default role).
    let rules = &server.config.auth.roles;
    let open_sign_up = server.config.mode == Mode::Hosted
        && rules.allowed_domains.is_empty()
        && rules.default_role.is_none()
        && rules.groups.is_empty();
    if !open_sign_up && server.config.server_role(&identity).is_none() {
        server.record(
            Event::ok(
                Actor { kind: "user".into(), id: identity.subject.clone(), email: identity.email.clone(), role: None },
                "auth.sign_in",
            )
            .outcome("denied"),
        );
        return back(&[
            ("error", "access_denied"),
            ("error_description", "this server does not let this account in"),
            ("state", &pending.client_state),
        ]);
    }
    let issuer = signer.issuer.as_str();
    let name = identity.name.clone().or_else(|| identity.email.clone()).unwrap_or_else(|| "Graphrt user".into());
    let account = server.accounts.upsert(NewIdentity {
        kind: AccountKind::Oidc,
        subject: format!("{issuer} {}", identity.subject),
        email: identity.email.clone(),
        name,
        groups: identity.groups.clone(),
    })?;
    server.record(Event::ok(actor(&account, None), "auth.sign_in"));
    let one_time = random_token()?;
    server.add_code(
        sha256_hex(one_time.as_bytes()),
        AuthCode {
            account: account.id.clone(),
            redirect_uri: pending.redirect_uri.clone(),
            challenge: pending.client_challenge.clone(),
            device: pending.device.clone(),
            expires_ms: 0,
        },
    );
    back(&[("code", &one_time), ("state", &pending.client_state)])
}

/// `true`, or the text `true` (some providers send boolean claims as text).
fn claim_is_true(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
enum TokenRequest {
    AuthorizationCode { code: String, code_verifier: String, redirect_uri: String },
    RefreshToken { refresh_token: String },
}

fn tokens(server: &Server, account: &Account, refresh: &str) -> ApiResult {
    let (access, expires_in) = server.access_token(&account.id, None)?;
    ok(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": expires_in,
        "refresh_token": refresh,
        "account": account.public(),
    }))
}

/// Trade a sign in code or a refresh token for tokens.
async fn token(State(server): State<Arc<Server>>, request: Request) -> ApiResult {
    let ip = client_ip(&server, &request);
    if !server.sign_in_attempts.allow(&ip) {
        return Err(ServerError::TooMany("too many sign in attempts, wait a minute".into()).into());
    }
    let Json(body) = Json::<TokenRequest>::from_request_body(request).await?;
    match body {
        TokenRequest::AuthorizationCode { code, code_verifier, redirect_uri } => {
            let entry = server
                .codes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&sha256_hex(code.as_bytes()));
            let entry = entry.filter(|c| c.expires_ms > now_ms()).ok_or_else(|| {
                ServerError::Unauthorized("this sign in code expired or was used: sign in again".into())
            })?;
            let challenge = graphrt_console::oidc::pkce_challenge(&code_verifier);
            if entry.redirect_uri != redirect_uri
                || !super::util::constant_time_eq(challenge.as_bytes(), entry.challenge.as_bytes())
            {
                return Err(ServerError::Unauthorized("the code verifier or redirect_uri does not match".into()).into());
            }
            let refresh = server.accounts.sign_in(&entry.account, &entry.device)?;
            let account = server
                .accounts
                .get(&entry.account)
                .ok_or_else(|| ServerError::Unauthorized("no such account".into()))?;
            tokens(&server, &account, &refresh)
        }
        TokenRequest::RefreshToken { refresh_token } => {
            let (account, refresh) = server.accounts.refresh(&refresh_token).inspect_err(|error| {
                if error.to_string().contains("two places") {
                    server.record(Event::ok(Actor::system(), "auth.refresh_reuse").outcome("denied"));
                }
            })?;
            tokens(&server, &account, &refresh)
        }
    }
}

trait FromBody: Sized {
    async fn from_request_body(request: Request) -> Result<Self, ApiError>;
}

impl<T: serde::de::DeserializeOwned> FromBody for Json<T> {
    async fn from_request_body(request: Request) -> Result<Self, ApiError> {
        let bytes = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|_| ServerError::BadRequest("the body is too large".into()))?;
        serde_json::from_slice(&bytes)
            .map(Json)
            .map_err(|error| ServerError::BadRequest(format!("the body is not valid: {error}")).into())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedRequest {
    workspace: String,
}

/// An access token for one workspace (sync sessions need one).
async fn scoped(State(server): State<Arc<Server>>, headers: HeaderMap, Json(body): Json<ScopedRequest>) -> ApiResult {
    let caller = server.caller(&headers)?;
    if caller.claims.ws.is_some() {
        return Err(ServerError::Forbidden("use an account access token to get a workspace token".into()).into());
    }
    let member = server.member(&caller, &body.workspace, Role::Viewer, "auth.scoped")?;
    let (token, expires_in) = server.access_token(&caller.account.id, Some(&member.workspace.id))?;
    ok(json!({ "access_token": token, "token_type": "Bearer", "expires_in": expires_in, "role": member.role }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogoutRequest {
    refresh_token: String,
}

async fn logout(State(server): State<Arc<Server>>, Json(body): Json<LogoutRequest>) -> ApiResult {
    let ended = server.accounts.sign_out(&body.refresh_token)?;
    ok(json!({ "signed_out": ended }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeRequest {
    code: String,
}

/// What an invite is for, before redeeming it (the workspace name and the role).
async fn preview_invite(State(server): State<Arc<Server>>, request: Request) -> ApiResult {
    let ip = client_ip(&server, &request);
    if !server.sign_in_attempts.allow(&ip) {
        return Err(ServerError::TooMany("too many attempts, wait a minute".into()).into());
    }
    let Json(body) = Json::<CodeRequest>::from_request_body(request).await?;
    let code = extract_code(&body.code);
    let invalid = || ServerError::NotFound("this invite is not valid (used, expired or revoked)".into());
    let ws = super::workspaces::Workspaces::invite_target(code).ok_or_else(invalid)?;
    let workspace = server.workspaces.get(ws).ok_or_else(invalid)?;
    let hash = sha256_hex(code.as_bytes());
    let invite = workspace.invites.iter().find(|i| i.matches(&hash) && i.usable(now_ms())).ok_or_else(invalid)?;
    let inviter = server.accounts.get(&invite.created_by).map(|a| a.name);
    ok(json!({
        "workspace": { "id": workspace.id, "name": workspace.name },
        "role": invite.role,
        "invited_by": inviter,
        "needs_sign_in": !server.config.auth.invite_accounts(),
        "email": invite.email,
    }))
}

/// The code of an invite link (`.../join#<code>`) or the code itself.
fn extract_code(text: &str) -> &str {
    let text = text.trim();
    text.rsplit_once('#').map_or(text, |(_, code)| code).trim()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RedeemRequest {
    code: String,
    /// The name to show, for a new account made by the invite.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    device: Option<String>,
}

/// Join a workspace with an invite code. Signed in: the account joins.
/// Signed out (on servers where invite codes make accounts): a new account
/// is made and signed in.
async fn redeem_invite(State(server): State<Arc<Server>>, request: Request) -> ApiResult {
    let ip = client_ip(&server, &request);
    if !server.sign_in_attempts.allow(&ip) {
        return Err(ServerError::TooMany("too many attempts, wait a minute".into()).into());
    }
    let headers = request.headers().clone();
    let Json(body) = Json::<RedeemRequest>::from_request_body(request).await?;
    let code = extract_code(&body.code).to_owned();
    let max_members = server.config.limits.members_per_workspace;
    let signed_in = match bearer(&headers) {
        Some(token) => Some(server.caller_of(token)?),
        None => None,
    };
    let (account, refresh) = match signed_in {
        Some(caller) => (caller.account, None),
        None => {
            if !server.config.auth.invite_accounts() {
                return Err(ServerError::Unauthorized("sign in first, then join with the invite".into()).into());
            }
            let name = super::workspaces::clean_name(body.name.as_deref().unwrap_or(""))
                .map_err(|_| ServerError::BadRequest("your name is needed (1 to 120 characters)".into()))?;
            // Check the invite before making an account for it.
            let ws = super::workspaces::Workspaces::invite_target(&code)
                .ok_or_else(|| ServerError::NotFound("this invite is not valid (used, expired or revoked)".into()))?;
            let hash = sha256_hex(code.as_bytes());
            let usable = server
                .workspaces
                .get(ws)
                .is_some_and(|w| w.invites.iter().any(|i| i.matches(&hash) && i.usable(now_ms()) && i.email.is_none()));
            if !usable {
                return Err(ServerError::NotFound("this invite is not valid (used, expired or revoked)".into()).into());
            }
            let account = server.accounts.upsert(NewIdentity {
                kind: AccountKind::Invite,
                subject: String::new(),
                email: None,
                name,
                groups: Vec::new(),
            })?;
            let refresh = server.accounts.sign_in(&account.id, body.device.as_deref().unwrap_or(""))?;
            server.record(Event::ok(actor(&account, None), "auth.invite_account"));
            (account, Some(refresh))
        }
    };
    let (workspace, role) = server.workspaces.redeem(&code, &account.id, account.email.as_deref(), max_members)?;
    if let Ok(files) = server.workspaces.files(&workspace.id) {
        files.record(Event::ok(actor(&account, Some(role)), "member.join").target(account.id.clone()));
    }
    server.membership_changed(&workspace.id, &account.id, Some(role));
    server.emit_event(&workspace.id, "member.joined", None, json!({ "account": account.public(), "role": role }));
    let mut answer = json!({
        "workspace": { "id": workspace.id, "name": workspace.name },
        "role": role,
        "account": account.public(),
    });
    if let Some(refresh) = refresh {
        let (access, expires_in) = server.access_token(&account.id, None)?;
        set(&mut answer, "refresh_token", json!(refresh));
        set(&mut answer, "access_token", json!(access));
        set(&mut answer, "expires_in", json!(expires_in));
    }
    ok(answer)
}

fn workspace_summary(workspace: &Workspace, role: Role) -> Value {
    json!({
        "id": workspace.id,
        "name": workspace.name,
        "role": role,
        "members": workspace.members.len(),
        "projects": workspace.projects.len(),
        "created_ms": workspace.created_ms,
    })
}

async fn me(State(server): State<Arc<Server>>, headers: HeaderMap) -> ApiResult {
    let caller = server.caller(&headers)?;
    let workspaces: Vec<Value> =
        server.workspaces.of_account(&caller.account.id).iter().map(|(w, r)| workspace_summary(w, *r)).collect();
    let sign_ins: Vec<Value> = server
        .accounts
        .sign_ins(&caller.account.id)
        .into_iter()
        .map(|(device, created_ms, expires_ms)| json!({ "device": device, "created_ms": created_ms, "expires_ms": expires_ms }))
        .collect();
    ok(json!({
        "account": caller.account.public(),
        "workspaces": workspaces,
        "may_create_workspaces": server.may_create(&caller.account),
        "sign_ins": sign_ins,
    }))
}

impl Server {
    /// Whether `account` may create workspaces (hosted: within its quota;
    /// self hosted: when the role rules make it an operator or owner, or it owns a workspace).
    fn may_create(&self, account: &Account) -> bool {
        match self.config.mode {
            Mode::Hosted => self.workspaces.created_by(&account.id) < self.config.limits.workspaces_per_account,
            Mode::SelfHosted => {
                self.config.server_role(&account.identity()).is_some_and(|r| r >= Role::Operator)
                    || self.workspaces.of_account(&account.id).iter().any(|(_, role)| *role == Role::Owner)
            }
        }
    }
}

async fn list_workspaces(State(server): State<Arc<Server>>, headers: HeaderMap) -> ApiResult {
    let caller = server.caller(&headers)?;
    let list: Vec<Value> =
        server.workspaces.of_account(&caller.account.id).iter().map(|(w, r)| workspace_summary(w, *r)).collect();
    ok(json!({ "workspaces": list }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NameRequest {
    name: String,
}

async fn create_workspace(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Json(body): Json<NameRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    if !server.may_create(&caller.account) {
        server.record(Event::ok(actor(&caller.account, None), "workspace.create").outcome("denied").status(403));
        return Err(match server.config.mode {
            Mode::Hosted => ServerError::Limit(format!(
                "an account creates at most {} workspaces",
                server.config.limits.workspaces_per_account
            )),
            Mode::SelfHosted => ServerError::Forbidden("ask an owner of this server to create workspaces".into()),
        }
        .into());
    }
    let workspace = server.workspaces.create(&body.name, &caller.account.id)?;
    server.record(Event::ok(actor(&caller.account, None), "workspace.create").target(workspace.id.clone()));
    if let Ok(files) = server.workspaces.files(&workspace.id) {
        files.record(
            Event::ok(actor(&caller.account, Some(Role::Owner)), "workspace.create").target(workspace.id.clone()),
        );
    }
    let mut response = Json(workspace_summary(&workspace, Role::Owner)).into_response();
    *response.status_mut() = StatusCode::CREATED;
    Ok(response)
}

async fn get_workspace(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "workspace.read")?;
    let mut summary = workspace_summary(&member.workspace, member.role);
    set(&mut summary, "members_list", members_json(&server, &member.workspace));
    set(&mut summary, "projects_list", projects_json(&server, &member.workspace));
    ok(summary)
}

fn members_json(server: &Server, workspace: &Workspace) -> Value {
    Value::Array(
        workspace
            .members
            .iter()
            .map(|(id, member)| {
                let mut entry = server.accounts.get(id).map_or_else(|| json!({ "id": id, "name": id }), |a| a.public());
                set(&mut entry, "role", json!(member.role));
                set(&mut entry, "added_ms", json!(member.added_ms));
                entry
            })
            .collect(),
    )
}

fn projects_json(server: &Server, workspace: &Workspace) -> Value {
    Value::Array(
        workspace
            .projects
            .iter()
            .map(|(id, info)| {
                let online = server.rooms.get(&workspace.id, id).map(|room| room.people()).unwrap_or_default();
                json!({
                    "id": id,
                    "name": info.name,
                    "created_ms": info.created_ms,
                    "updated_ms": info.updated_ms,
                    "online": online,
                })
            })
            .collect(),
    )
}

async fn rename_workspace(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(ws): Path<String>,
    Json(body): Json<NameRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "workspace.rename")?;
    server.workspaces.rename(&ws, &body.name)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "workspace.rename").target(ws));
    ok(json!({ "renamed": true }))
}

async fn delete_workspace(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "workspace.delete")?;
    for project in member.workspace.projects.keys() {
        server.rooms.close_now(&ws, project, "the workspace was deleted");
    }
    server.workspaces.delete(&ws)?;
    server.record(Event::ok(actor(&caller.account, Some(member.role)), "workspace.delete").target(ws));
    ok(json!({ "deleted": true }))
}

async fn list_members(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "member.list")?;
    ok(json!({ "members": members_json(&server, &member.workspace) }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleRequest {
    role: Role,
}

async fn set_member(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, account)): Path<(String, String)>,
    Json(body): Json<RoleRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "member.role")?;
    // Only existing members change role here: new people join with an invite.
    if !member.workspace.members.contains_key(&account) {
        return Err(ServerError::NotFound("not a member: invite them".into()).into());
    }
    server.workspaces.set_role(
        &ws,
        &account,
        body.role,
        &caller.account.id,
        server.config.limits.members_per_workspace,
    )?;
    member.files.record(
        Event::ok(actor(&caller.account, Some(member.role)), "member.role")
            .target(account.clone())
            .details(json!({ "role": body.role })),
    );
    server.membership_changed(&ws, &account, Some(body.role));
    ok(json!({ "role": body.role }))
}

async fn remove_member(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, account)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    // Anyone may leave. Removing someone else takes an owner.
    let min = if account == caller.account.id { Role::Viewer } else { Role::Owner };
    let member = server.member(&caller, &ws, min, "member.remove")?;
    server.workspaces.remove_member(&ws, &account)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "member.remove").target(account.clone()));
    server.membership_changed(&ws, &account, None);
    ok(json!({ "removed": true }))
}

async fn list_invites(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "invite.list")?;
    let now = now_ms();
    let invites: Vec<Value> =
        member.workspace.invites.iter().filter(|i| i.usable(now)).map(super::workspaces::Invite::public).collect();
    ok(json!({ "invites": invites }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InviteRequest {
    role: Role,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    max_uses: Option<u32>,
    #[serde(default)]
    expires_hours: Option<u64>,
}

async fn create_invite(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(ws): Path<String>,
    Json(body): Json<InviteRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "invite.create")?;
    let (invite, code) = server.workspaces.invite(
        &ws,
        body.role,
        body.email,
        body.max_uses.unwrap_or(1),
        body.expires_hours.unwrap_or(DEFAULT_INVITE_HOURS),
        &caller.account.id,
    )?;
    member.files.record(
        Event::ok(actor(&caller.account, Some(member.role)), "invite.create")
            .target(invite.id.clone())
            .details(json!({ "role": invite.role, "max_uses": invite.max_uses, "email": invite.email })),
    );
    let mut answer = invite.public();
    set(&mut answer, "code", json!(code));
    set(&mut answer, "link", json!(server.invite_link(&code)));
    let mut response = Json(answer).into_response();
    *response.status_mut() = StatusCode::CREATED;
    Ok(response)
}

async fn revoke_invite(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, invite)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "invite.revoke")?;
    server.workspaces.revoke_invite(&ws, &invite)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "invite.revoke").target(invite));
    ok(json!({ "revoked": true }))
}

async fn list_projects(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "project.list")?;
    ok(json!({ "projects": projects_json(&server, &member.workspace) }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    name: String,
    /// The first content (sections), e.g. a project shared from the app.
    #[serde(default)]
    content: Option<serde_json::Map<String, Value>>,
}

async fn create_project(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(ws): Path<String>,
    Json(body): Json<NewProject>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Operator, "project.create")?;
    let project = server.workspaces.add_project(
        &ws,
        &body.name,
        &caller.account.id,
        server.config.limits.projects_per_workspace,
    )?;
    if let Some(content) = body.content {
        let room = server.rooms.get_or_open(&ws, &project, member.files.project_dir(&project))?;
        let result = room.replace_json(&content, &caller.account.name);
        server.rooms.close_when_idle(&room);
        if let Err(error) = result {
            let _undo = server.workspaces.remove_project(&ws, &project);
            server.rooms.close_now(&ws, &project, "");
            let _cleanup = std::fs::remove_dir_all(member.files.project_dir(&project));
            return Err(error.into());
        }
    }
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "project.create").target(project.clone()));
    let mut response = Json(json!({ "id": project, "name": body.name.trim() })).into_response();
    *response.status_mut() = StatusCode::CREATED;
    Ok(response)
}

fn check_project(member: &Member, project: &str) -> ServerResult<()> {
    if !is_id(project, "pr_") || !member.workspace.projects.contains_key(project) {
        return Err(ServerError::NotFound("no such project".into()));
    }
    Ok(())
}

async fn get_project(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "project.read")?;
    check_project(&member, &project)?;
    let room = server.rooms.get_or_open(&ws, &project, member.files.project_dir(&project))?;
    let content = room.json();
    let online = room.people();
    if room.peers() == 0 {
        server.rooms.close_when_idle(&room);
    }
    let info = member.workspace.projects.get(&project);
    ok(json!({
        "id": project,
        "name": info.map(|i| i.name.clone()),
        "updated_ms": info.map(|i| i.updated_ms),
        "online": online,
        "content": content,
        "role": member.role,
    }))
}

async fn rename_project(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
    Json(body): Json<NameRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Operator, "project.rename")?;
    check_project(&member, &project)?;
    server.workspaces.rename_project(&ws, &project, &body.name)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "project.rename").target(project));
    ok(json!({ "renamed": true }))
}

async fn delete_project(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "project.delete")?;
    check_project(&member, &project)?;
    server.rooms.close_now(&ws, &project, "the project was deleted");
    server.workspaces.remove_project(&ws, &project)?;
    let dir = member.files.project_dir(&project);
    if let Err(error) = std::fs::remove_dir_all(&dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("warning: collab: cannot remove {}: {error}", dir.display());
    }
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "project.delete").target(project));
    ok(json!({ "deleted": true }))
}

async fn list_versions(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "version.list")?;
    check_project(&member, &project)?;
    ok(json!({ "versions": room::versions(&member.files.project_dir(&project))? }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelRequest {
    #[serde(default)]
    label: String,
}

async fn save_version(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
    Json(body): Json<LabelRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Operator, "version.save")?;
    check_project(&member, &project)?;
    let room = server.rooms.get_or_open(&ws, &project, member.files.project_dir(&project))?;
    let version = room.save_version(&body.label, &caller.account.name)?;
    room.broadcast_all(Frame::Control(Control::VersionSaved {
        version: version.version,
        label: version.label.clone(),
        by: caller.account.name.clone(),
    }));
    if room.peers() == 0 {
        server.rooms.close_when_idle(&room);
    }
    server.emit_event(
        &ws,
        "version.saved",
        Some(&project),
        json!({ "version": version.version, "label": version.label, "by": caller.account.name }),
    );
    member.files.record(
        Event::ok(actor(&caller.account, Some(member.role)), "version.save")
            .target(project)
            .details(json!({ "version": version.version, "label": version.label })),
    );
    ok(serde_json::to_value(version).map_err(|e| ServerError::Internal(e.to_string()))?)
}

async fn get_version(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project, version)): Path<(String, String, u64)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "version.read")?;
    check_project(&member, &project)?;
    let doc = room::version_doc(&member.files.project_dir(&project), version)?;
    ok(json!({ "version": version, "content": doc.to_json() }))
}

/// Make the project equal an earlier version. The restore is an edit like
/// any other: people editing at the same time merge with it, and it can be undone
/// by restoring the version before it.
async fn restore_version(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project, version)): Path<(String, String, u64)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Operator, "version.restore")?;
    check_project(&member, &project)?;
    let dir = member.files.project_dir(&project);
    let content = room::version_doc(&dir, version)?.to_json();
    let Value::Object(content) = content else {
        return Err(ServerError::Internal("a version is not an object".into()).into());
    };
    let room = server.rooms.get_or_open(&ws, &project, dir)?;
    // Keep what was there before as a version too, so the restore can be undone.
    room.save_version(&format!("before restoring version {version}"), &caller.account.name)?;
    let changed = room.replace_json(&content, &caller.account.name)?;
    if room.peers() == 0 {
        server.rooms.close_when_idle(&room);
    }
    if changed {
        server.workspaces.touch_project(&ws, &project);
        server.project_changed(&ws, &project, &caller.account.id);
    }
    member.files.record(
        Event::ok(actor(&caller.account, Some(member.role)), "version.restore")
            .target(project)
            .details(json!({ "version": version })),
    );
    ok(json!({ "restored": version, "changed": changed }))
}

async fn audit(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(ws): Path<String>,
    Query(query): Query<graphrt_console::audit::Query>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "audit.read")?;
    let page = member.files.audit.query(&query).map_err(ServerError::from)?;
    ok(serde_json::to_value(page).map_err(|e| ServerError::Internal(e.to_string()))?)
}

async fn list_secrets(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "secret.list")?;
    let secrets = member.files.secrets.list().map_err(ServerError::from)?;
    let list: Vec<Value> = secrets
        .into_iter()
        .map(|s| json!({ "name": s.name, "updated_ms": s.updated_ms, "updated_by": s.updated_by }))
        .collect();
    ok(json!({ "secrets": list, "can_set": member.files.secrets.has_key() }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRequest {
    value: String,
}

async fn set_secret(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, name)): Path<(String, String)>,
    Json(body): Json<SecretRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "secret.set")?;
    let existed = member.files.secrets.set(&name, &body.value, &caller.account.name).map_err(ServerError::from)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "secret.set").target(name));
    ok(json!({ "replaced": existed }))
}

async fn remove_secret(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, name)): Path<(String, String)>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "secret.remove")?;
    let existed = member.files.secrets.remove(&name).map_err(ServerError::from)?;
    member.files.record(Event::ok(actor(&caller.account, Some(member.role)), "secret.remove").target(name));
    ok(json!({ "removed": existed }))
}

async fn list_services(State(server): State<Arc<Server>>, headers: HeaderMap, Path(ws): Path<String>) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "service.list")?;
    #[cfg(feature = "services")]
    let list = server.services.list(&member.workspace);
    #[cfg(not(feature = "services"))]
    let list: Vec<Value> = {
        let _ = &member;
        Vec::new()
    };
    ok(json!({ "services": list }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(feature = "services"), allow(dead_code))]
struct GrantRequest {
    enabled: bool,
    #[serde(default)]
    secrets: Vec<String>,
}

async fn grant_service(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, plugin, service)): Path<(String, String, String)>,
    Json(body): Json<GrantRequest>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Owner, "service.grant")?;
    #[cfg(feature = "services")]
    {
        let grant = server.services.grant(
            &server,
            &member.workspace,
            &plugin,
            &service,
            body.enabled,
            body.secrets,
            &caller.account.name,
        )?;
        member.files.record(
            Event::ok(actor(&caller.account, Some(member.role)), "service.grant")
                .target(format!("{plugin}/{service}"))
                .details(json!({ "enabled": grant.enabled, "secrets": grant.secrets })),
        );
        ok(serde_json::to_value(grant).map_err(|e| ServerError::Internal(e.to_string()))?)
    }
    #[cfg(not(feature = "services"))]
    {
        let _ = (member, plugin, service, body);
        Err(ServerError::NotFound("this server has no plugin services".into()).into())
    }
}

async fn call_service(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, plugin, service)): Path<(String, String, String)>,
    Json(input): Json<Value>,
) -> ApiResult {
    let caller = server.caller(&headers)?;
    let member = server.member(&caller, &ws, Role::Viewer, "service.call")?;
    #[cfg(feature = "services")]
    {
        let output = server
            .services
            .call(&server, &member.workspace, member.role, &member.files, &caller.account, &plugin, &service, input)
            .await?;
        ok(output)
    }
    #[cfg(not(feature = "services"))]
    {
        let _ = (member, plugin, service, input);
        Err(ServerError::NotFound("this server has no plugin services".into()).into())
    }
}

/// The access token of a sync request: `Authorization: Bearer`, else the
/// `bearer.<token>` WebSocket subprotocol (browsers cannot set headers).
fn sync_token(headers: &HeaderMap) -> Option<String> {
    if let Some(token) = bearer(headers) {
        return Some(token.to_owned());
    }
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find_map(|p| p.strip_prefix(BEARER_SUBPROTOCOL_PREFIX))
        .map(str::to_owned)
}

/// Open a sync session (WebSocket) on a project.
async fn sync_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((ws, project)): Path<(String, String)>,
    upgrade: WebSocketUpgrade,
) -> ApiResult {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let allowed = origin == server.public.origin || server.config.allowed_origins.iter().any(|o| o == origin);
        if !allowed {
            return Err(ServerError::Forbidden("this web origin may not open sync sessions".into()).into());
        }
    }
    let token = sync_token(&headers).ok_or_else(|| ServerError::Unauthorized("sign in first".into()))?;
    let caller = server.caller_of(&token)?;
    if caller.claims.ws.as_deref() != Some(ws.as_str()) {
        return Err(ServerError::Unauthorized("a sync session needs an access token for this workspace".into()).into());
    }
    let member = match server.member(&caller, &ws, Role::Viewer, "project.join") {
        Ok(member) => member,
        Err(error) => {
            server.record(
                Event::ok(actor(&caller.account, None), "project.join")
                    .target(format!("{ws}/{project}"))
                    .outcome("denied")
                    .status(error.status()),
            );
            return Err(error.into());
        }
    };
    check_project(&member, &project)?;
    server.begin_session(&caller.account.id)?;
    let room = match server.rooms.get_or_open(&ws, &project, member.files.project_dir(&project)) {
        Ok(room) => room,
        Err(error) => {
            server.end_session(&caller.account.id);
            return Err(error.into());
        }
    };
    let session = sync::Session {
        account: caller.account.clone(),
        role: member.role,
        workspace: ws,
        project,
        expires_secs: caller.claims.exp,
        room,
    };
    let max = server.config.limits.max_message_bytes;
    let failed = Arc::clone(&server);
    let account = caller.account.id.clone();
    let running = Arc::clone(&server);
    Ok(upgrade
        .protocols([SUBPROTOCOL])
        .max_message_size(max)
        .max_frame_size(max)
        .on_failed_upgrade(move |_| failed.end_session(&account))
        .on_upgrade(move |socket| sync::run(running, socket, session))
        .into_response())
}

impl super::room::Room {
    /// Make the project equal `content` as an edit of `editor`: stored and sent to everyone.
    ///
    /// @returns Whether anything changed.
    /// @throws ServerError - `BadRequest` for content that nests too deep, `Limit` over the size limit.
    pub fn replace_json(&self, content: &serde_json::Map<String, Value>, editor: &str) -> ServerResult<bool> {
        let scratch = crate::ProjectDoc::new();
        scratch.apply_update(&self.encode_state()).map_err(|e| ServerError::Internal(e.to_string()))?;
        let update = scratch.apply_json(content).map_err(|e| ServerError::BadRequest(e.to_string()))?;
        match update {
            Some(update) => self.apply(None, editor, &update).map(|applied| applied == super::room::Applied::Yes),
            None => Ok(false),
        }
    }
}

impl super::workspaces::Workspaces {
    /// Every workspace id and name.
    pub fn all(&self) -> Vec<(String, String)> {
        let mut ids: Vec<(String, String)> = Vec::new();
        for id in self.ids() {
            if let Some(w) = self.get(&id) {
                ids.push((w.id, w.name));
            }
        }
        ids
    }
}

/// Close every session of `account` (an account disabled by the server's admin).
pub fn disconnect(server: &Server, account: &str) {
    for (workspace, _) in server.workspaces.all() {
        for room in server.rooms.of_workspace(&workspace) {
            room.notify(Some(account), &Outgoing::Close("this account was disabled".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_redirects_only() {
        assert!(is_loopback_redirect("http://127.0.0.1:53111/callback"));
        assert!(is_loopback_redirect("http://localhost:8000/cb"));
        assert!(is_loopback_redirect("http://[::1]:9/x"));
        assert!(!is_loopback_redirect("https://127.0.0.1/cb"));
        assert!(!is_loopback_redirect("http://evil.example/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1.evil.example/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1@evil.example/cb"));
        assert!(!is_loopback_redirect("myapp://cb"));
    }

    #[test]
    fn codes_from_links() {
        assert_eq!(extract_code("https://collab.example.com/join#gci1.ws_x.y"), "gci1.ws_x.y");
        assert_eq!(extract_code(" gci1.ws_x.y "), "gci1.ws_x.y");
    }

    #[test]
    fn sync_tokens_from_headers() {
        let mut headers = HeaderMap::new();
        headers
            .insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("graphrt-collab.v1, bearer.gca1.abc.def"));
        assert_eq!(sync_token(&headers).as_deref(), Some("gca1.abc.def"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t2"));
        assert_eq!(sync_token(&headers).as_deref(), Some("t2"));
    }

    #[test]
    fn escapes_html_and_forms() {
        assert_eq!(html_escape("<a href=\"x\">&"), "&lt;a href=&quot;x&quot;&gt;&amp;");
        assert_eq!(form_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
