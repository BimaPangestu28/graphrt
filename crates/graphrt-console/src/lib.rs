//! # graphrt-console
//!
//! An admin console for teams that run `graphrt serve` on their own servers
//! (on premises, in a VPC, air gapped), behind `graphrt serve --console FILE`
//! and the cargo feature `console` of `graphrt-cli`. Off unless configured.
//!
//! - **Sign in with OpenID Connect** ([`oidc`]): Entra ID, Google Workspace,
//!   Okta, Keycloak or any other provider; browser sessions in `HttpOnly`
//!   `SameSite=Strict` cookies with CSRF tokens and a `Host` allowlist
//!   ([`sessions`]). The API key keeps working for automation.
//! - **Roles** ([`roles`], [`policy`]): owner, operator and viewer, mapped
//!   from the token's groups or set by an owner, checked on every route.
//! - **An audit log** ([`audit`]): append only and hash chained, queryable,
//!   exported as JSON lines, with retention.
//! - **Secrets** ([`secrets`]): environment variables of the served
//!   workflows, AES-256-GCM encrypted at rest, never shown again.
//! - **An agent registry** ([`registry`]): versions of each workflow by
//!   content hash, environments, promotion and rollback.
//!
//! The host (`graphrt serve`) puts [`http::gate`] in front of its router and
//! merges [`http::routes`]; it gives the console what it knows of the
//! served workflows through [`Host`].

pub mod audit;
pub mod config;
mod error;
pub mod http;
pub mod oidc;
pub mod policy;
pub mod registry;
pub mod roles;
pub mod secrets;
pub mod sessions;
mod util;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

pub use config::ConsoleConfig;
pub use error::{ConsoleError, Result};
use futures::future::BoxFuture;
pub use registry::Format;
pub use roles::Role;

use crate::audit::{Actor, AuditLog, Event};
use crate::config::PublicUrl;
use crate::oidc::{Provider, VerifierSource};
use crate::registry::{Registry, Version};
use crate::roles::Members;
use crate::secrets::{SecretKey, Secrets};
use crate::sessions::{PendingLogins, Sessions};

/// A workflow the server serves, as it was loaded at startup.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ServedWorkflow {
    pub name: String,
    /// SHA-256 of the file it was loaded from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Its registry version, when it came from the registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The environment variables it declares.
    pub env: Vec<String>,
}

/// What the console needs from the server it runs in.
pub trait Host: Send + Sync + std::fmt::Debug {
    /// The workflows served now.
    fn served(&self) -> Vec<ServedWorkflow>;

    /// Check that `bytes` compile as a workflow of this server.
    ///
    /// @returns The warnings.
    /// @throws String - Why it does not compile.
    fn validate<'a>(
        &'a self,
        name: &'a str,
        format: Format,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Vec<String>, String>>;

    /// Drain the runs and start again, so the registry and the secrets are
    /// read anew. Paused runs carry on after the restart.
    ///
    /// @throws String - When the server cannot restart itself.
    fn restart(&self) -> std::result::Result<(), String>;
}

/// What [`Console::open`] needs besides the config.
#[derive(Debug)]
pub struct Options {
    pub config: ConsoleConfig,
    /// Where the console keeps its files (the config's, else a default of the host).
    pub data_dir: PathBuf,
    /// The server's API key: what a signed in request is let through with.
    pub api_key: String,
    /// The key of the secrets, when one is configured.
    pub secret_key: Option<SecretKey>,
    /// The OIDC client secret, when one is configured.
    pub client_secret: Option<String>,
    /// Checks the signatures of ID tokens.
    pub verifiers: Arc<dyn VerifierSource>,
}

/// The console of one server.
#[derive(Debug)]
pub struct Console {
    config: ConsoleConfig,
    public: PublicUrl,
    environment: String,
    api_key: String,
    started_ms: u64,
    secrets_changed: AtomicBool,
    pub(crate) sessions: Sessions,
    pub(crate) members: Members,
    pub(crate) audit: AuditLog,
    pub(crate) secrets: Secrets,
    pub(crate) registry: Registry,
    pub(crate) provider: Provider,
    pub(crate) pending: PendingLogins,
    host: OnceLock<Arc<dyn Host>>,
}

impl Console {
    /// Open the console's files in `options.data_dir`.
    ///
    /// @throws ConsoleError - On an unusable config, a directory that cannot
    ///   be created, files that cannot be read, or secrets sealed with another key.
    pub fn open(options: Options) -> Result<Self> {
        let Options { config, data_dir, api_key, secret_key, client_secret, verifiers } = options;
        config.check()?;
        if api_key.is_empty() {
            return Err(ConsoleError::Config("the console needs the server's API key".into()));
        }
        util::ensure_dir(&data_dir)?;
        let public = config.public()?;
        let environment = config.environment().to_owned();
        let redirect_uri = format!("{}{}/console/callback", public.origin, public.prefix);
        Ok(Self {
            sessions: Sessions::open(&data_dir, &config.sessions)?,
            members: Members::open(&data_dir)?,
            audit: AuditLog::open(&data_dir, config.audit.retention_days)?,
            secrets: Secrets::open(&data_dir, secret_key)?,
            registry: Registry::open(&data_dir, config.environments.clone())?,
            provider: Provider::new(config.oidc.clone(), client_secret, redirect_uri, verifiers)?,
            pending: PendingLogins::default(),
            host: OnceLock::new(),
            started_ms: util::now_ms(),
            secrets_changed: AtomicBool::new(false),
            config,
            public,
            environment,
            api_key,
        })
    }

    /// The config.
    pub fn config(&self) -> &ConsoleConfig {
        &self.config
    }

    /// The environment this server serves.
    pub fn environment(&self) -> &str {
        &self.environment
    }

    pub(crate) fn public(&self) -> &PublicUrl {
        &self.public
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn host(&self) -> Option<&Arc<dyn Host>> {
        self.host.get()
    }

    /// Give the console the server it runs in (once).
    pub fn attach(&self, host: Arc<dyn Host>) {
        let _already = self.host.set(host);
    }

    /// The secrets as environment variables, for the served workflows.
    ///
    /// @throws ConsoleError::Crypto - When a secret cannot be decrypted.
    pub fn secret_values(&self) -> Result<Vec<(String, String)>> {
        self.secrets.values()
    }

    /// The registry versions this server's environment serves: `(name, version, file)`.
    ///
    /// @throws ConsoleError - When the registry cannot be read or a file is missing.
    pub fn served_files(&self) -> Result<Vec<(String, Version, PathBuf)>> {
        self.registry.served_files(&self.environment)
    }

    /// One line for the startup banner.
    pub fn describe(&self) -> String {
        let secrets = if self.secrets.has_key() { "secrets encrypted" } else { "no secrets key" };
        format!(
            "admin console at {}{}/console/ (sign in with {}, environment `{}`, {secrets}, audit kept {} days)",
            self.public.origin,
            self.public.prefix,
            self.provider.issuer(),
            self.environment,
            self.audit.retention_days(),
        )
    }

    /// Write `event` to the audit log. A failed write is reported on stderr:
    /// the request it records has happened already.
    pub fn record(&self, event: Event) {
        if let Err(error) = self.audit.append(event) {
            eprintln!("warning: console: cannot write the audit log: {error}");
        }
    }

    /// Prune the audit log now and then daily, for as long as the console lives.
    pub fn spawn_retention(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut daily = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                daily.tick().await;
                let Some(console) = weak.upgrade() else { return };
                match console.audit.prune() {
                    Ok(0) => {}
                    Ok(removed) => {
                        let details = serde_json::json!({ "days_removed": removed });
                        console.record(Event::ok(Actor::system(), "audit.retention").details(details));
                    }
                    Err(error) => eprintln!("warning: console: audit retention failed: {error}"),
                }
            }
        });
    }

    pub(crate) fn note_secret_change(&self) {
        self.secrets_changed.store(true, Ordering::Relaxed);
    }

    /// Whether something changed that applies at the next restart.
    pub(crate) fn pending_changes(&self) -> Result<Vec<String>> {
        let mut pending = Vec::new();
        let secrets_changed = self.secrets_changed.load(Ordering::Relaxed)
            || self.secrets.list()?.iter().any(|secret| secret.updated_ms > self.started_ms);
        if secrets_changed {
            pending.push("secrets".to_owned());
        }
        let served = self.host().map(|host| host.served()).unwrap_or_default();
        for (name, version, _) in self.served_files()? {
            let running = served.iter().find(|s| s.name == name);
            if running.and_then(|s| s.hash.as_deref()) != Some(version.hash.as_str()) {
                pending.push(format!("workflow:{name}"));
            }
        }
        Ok(pending)
    }
}

/// The data directory of a console whose config names none: `console/` in
/// the run store directory.
///
/// @throws ConsoleError::Config - Without either.
pub fn data_dir(config: &ConsoleConfig, store_dir: Option<&Path>) -> Result<PathBuf> {
    match (&config.data_dir, store_dir) {
        (Some(dir), _) => Ok(dir.clone()),
        (None, Some(store)) => Ok(store.join("console")),
        (None, None) => Err(ConsoleError::Config(
            "the console keeps its files on disk: set `data_dir` in the console config or serve with --store".into(),
        )),
    }
}
