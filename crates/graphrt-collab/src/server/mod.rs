//! The collaboration server (cargo feature `server`).
//!
//! One binary serves both ways Graphrt runs it:
//!
//! - **hosted** ([`Mode::Hosted`]): Graphrt's service, many teams on one
//!   server, open sign up through the identity provider, quotas per account;
//! - **self hosted** ([`Mode::SelfHosted`]): one company on its own server,
//!   sign in limited by the console's role rules (`auth.roles`).
//!
//! What it does: accounts and sign in ([`accounts`], OpenID Connect through
//! the console's provider code, or invite codes), team workspaces with the
//! console's roles ([`workspaces`]), live rooms that merge edits and show who
//! is there ([`room`], [`sync`]), versions, an audit log and secrets per
//! workspace (the console's), and plugin services ([`services`]).

pub mod accounts;
pub mod config;
mod error;
pub mod http;
pub mod rate;
pub mod room;
#[cfg(feature = "services")]
pub mod services;
mod sync;
pub mod tls;
pub mod token;
mod util;
pub mod workspaces;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use graphrt_console::audit::{Actor, AuditLog, Event};
use graphrt_console::oidc::{Provider, VerifierSource};

use self::accounts::Accounts;
use self::config::PublicUrl;
pub use self::config::{CollabConfig, Mode};
pub use self::error::{ServerError, ServerResult};
use self::rate::{Limiter, Rule};
use self::room::{RoomLimits, Rooms};
use self::token::{AccessClaims, SigningKey};
use self::util::{MS_PER_MINUTE, now_ms};
use self::workspaces::Workspaces;

/// Sign ins waiting for the identity provider, at most.
const MAX_PENDING_LOGINS: usize = 10_000;
/// How long a sign in may take at the provider.
const LOGIN_TTL_MS: u64 = 10 * MS_PER_MINUTE;
/// How long the one time code of a finished sign in lasts.
const CODE_TTL_MS: u64 = 60 * 1000;

/// What [`Server::open`] needs besides the config.
#[derive(Debug)]
pub struct Options {
    pub config: CollabConfig,
    /// The token signing key text, when one is configured (else a generated one in `data_dir`).
    pub signing_key: Option<String>,
    /// The key text of workspace secrets, when one is configured.
    pub secrets_key: Option<String>,
    /// The OIDC client secret, when one is configured.
    pub client_secret: Option<String>,
    /// Checks the signatures of ID tokens (needed with `auth.oidc`).
    pub verifiers: Option<Arc<dyn VerifierSource>>,
}

/// A sign in waiting for the identity provider.
#[derive(Debug)]
pub(crate) struct PendingLogin {
    pub nonce: String,
    pub verifier: String,
    /// The app's loopback address, its state and its PKCE challenge.
    pub redirect_uri: String,
    pub client_state: String,
    pub client_challenge: String,
    pub device: String,
    pub expires_ms: u64,
}

/// A finished sign in the app trades for tokens.
#[derive(Debug)]
pub(crate) struct AuthCode {
    pub account: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub device: String,
    pub expires_ms: u64,
}

/// The collaboration server.
#[derive(Debug)]
pub struct Server {
    pub(crate) config: CollabConfig,
    pub(crate) public: PublicUrl,
    pub(crate) accounts: Accounts,
    pub(crate) workspaces: Workspaces,
    pub(crate) rooms: Arc<Rooms>,
    pub(crate) signing: SigningKey,
    pub(crate) audit: AuditLog,
    pub(crate) provider: Option<Provider>,
    pub(crate) logins: Mutex<HashMap<String, PendingLogin>>,
    pub(crate) codes: Mutex<HashMap<String, AuthCode>>,
    pub(crate) requests: Limiter,
    pub(crate) sign_in_attempts: Limiter,
    pub(crate) sessions: Mutex<HashMap<String, usize>>,
    #[cfg(feature = "services")]
    pub(crate) services: services::Services,
}

impl Server {
    /// Open the server's files and check its config.
    ///
    /// @throws ServerError - On an unusable config or key, or files that cannot be read.
    pub fn open(options: Options) -> ServerResult<Arc<Self>> {
        let Options { config, signing_key, secrets_key, client_secret, verifiers } = options;
        config.check()?;
        let public = config.public()?;
        util::ensure_dir(&config.data_dir)?;
        let signing = match signing_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            Some(text) => SigningKey::parse(text)?,
            None => SigningKey::load_or_create(&config.data_dir)?,
        };
        let provider = match &config.auth.oidc {
            Some(oidc) => {
                let verifiers = verifiers.ok_or_else(|| {
                    ServerError::Config("auth.oidc needs an ID token verifier (built with the CLI's `jwt`)".into())
                })?;
                Some(Provider::new(oidc.clone(), client_secret, public.join("/v1/auth/callback"), verifiers)?)
            }
            None => None,
        };
        let limits = &config.limits;
        let rooms = Arc::new(Rooms::new(RoomLimits {
            max_doc_bytes: limits.max_doc_bytes,
            auto_versions: limits.auto_versions,
            version_every_ms: limits.version_every_minutes.max(1) * MS_PER_MINUTE,
        }));
        #[cfg(feature = "services")]
        let services = services::Services::load(&config.plugins)?;
        let server = Arc::new(Self {
            accounts: Accounts::open(&config.data_dir, config.auth.refresh_token_days)?,
            workspaces: Workspaces::open(&config.data_dir, config.audit.retention_days, secrets_key)?,
            audit: AuditLog::open(&config.data_dir, config.audit.retention_days)?,
            requests: Limiter::new(Rule::per_minute(limits.requests_per_minute)),
            sign_in_attempts: Limiter::new(Rule::per_minute(30)),
            rooms,
            signing,
            provider,
            logins: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            #[cfg(feature = "services")]
            services,
            public,
            config,
        });
        #[cfg(feature = "services")]
        server.services.attach(&server);
        Ok(server)
    }

    /// The config.
    pub fn config(&self) -> &CollabConfig {
        &self.config
    }

    /// One line for the startup banner.
    pub fn describe(&self) -> String {
        let sign_in = match (&self.config.auth.oidc, self.config.auth.invite_accounts()) {
            (Some(oidc), true) => format!("sign in with {} or invite codes", oidc.issuer),
            (Some(oidc), false) => format!("sign in with {}", oidc.issuer),
            (None, _) => "invite codes".to_owned(),
        };
        let mode = match self.config.mode {
            Mode::Hosted => "hosted",
            Mode::SelfHosted => "self hosted",
        };
        format!(
            "collaboration server at {} ({mode}, {sign_in}, {} workspaces, {} accounts)",
            self.public.join(""),
            self.workspaces.of_all(),
            self.accounts.len()
        )
    }

    /// Write `event` to the server's audit log (sign ins, workspaces made and deleted).
    pub(crate) fn record(&self, event: Event) {
        if let Err(error) = self.audit.append(event) {
            eprintln!("warning: collab: cannot write the audit log: {error}");
        }
    }

    /// A signed access token for `account`, scoped to `workspace` when given.
    pub(crate) fn access_token(&self, account: &str, workspace: Option<&str>) -> ServerResult<(String, u64)> {
        let now = now_ms() / 1000;
        let ttl = self.config.auth.access_token_minutes * 60;
        let claims =
            AccessClaims { sub: account.to_owned(), ws: workspace.map(str::to_owned), iat: now, exp: now + ttl };
        Ok((self.signing.sign(&claims)?, ttl))
    }

    /// Bootstrap without the HTTP API (the `graphrt collab invite` command):
    /// create workspace `name` when there is none of that name, and an invite to it.
    ///
    /// @returns The workspace id and the invite code.
    /// @throws ServerError - When the files cannot be written.
    pub fn bootstrap_invite(
        &self,
        name: &str,
        role: graphrt_console::Role,
        email: Option<String>,
        expires_hours: u64,
    ) -> ServerResult<(String, String)> {
        let existing = self.workspaces.by_name(name);
        let workspace = match existing {
            Some(id) => id,
            None => {
                // Owned by nobody until the invite is redeemed: the first owner joins with it.
                let workspace = self.workspaces.create_unowned(name)?;
                self.record(Event::ok(Actor::system(), "workspace.create").target(workspace.clone()));
                workspace
            }
        };
        let (invite, code) = self.workspaces.invite(&workspace, role, email, 1, expires_hours, "system")?;
        if let Ok(files) = self.workspaces.files(&workspace) {
            files.record(
                Event::ok(Actor::system(), "invite.create")
                    .target(invite.id)
                    .details(serde_json::json!({ "role": role })),
            );
        }
        Ok((workspace, code))
    }

    /// The link that carries an invite code (`<public url>/join#<code>`).
    pub fn invite_link(&self, code: &str) -> String {
        format!("{}#{code}", self.public.join("/join"))
    }

    /// Save every open project and workspace (at shutdown).
    pub fn shutdown(&self) {
        self.rooms.checkpoint_all();
        self.workspaces.flush();
    }

    /// Forget expired sign ins and codes.
    pub(crate) fn prune_logins(&self) {
        let now = now_ms();
        self.logins.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|_, l| l.expires_ms > now);
        self.codes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|_, c| c.expires_ms > now);
    }

    pub(crate) fn add_login(&self, state: String, mut login: PendingLogin) -> ServerResult<()> {
        self.prune_logins();
        let mut logins = self.logins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if logins.len() >= MAX_PENDING_LOGINS {
            return Err(ServerError::TooMany("too many sign ins in progress, try again in a minute".into()));
        }
        login.expires_ms = now_ms() + LOGIN_TTL_MS;
        logins.insert(state, login);
        Ok(())
    }

    pub(crate) fn add_code(&self, code_hash: String, mut code: AuthCode) {
        code.expires_ms = now_ms() + CODE_TTL_MS;
        self.codes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(code_hash, code);
    }
}
