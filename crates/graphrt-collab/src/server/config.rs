//! The collaboration server's config: one YAML (or JSON) file.
//!
//! ```yaml
//! mode: self_hosted                 # or `hosted` (many teams, open sign up)
//! public_url: https://collab.example.com
//! listen: 0.0.0.0:8443
//! data_dir: /var/lib/graphrt-collab
//! tls: { cert: /etc/graphrt/tls.crt, key: /etc/graphrt/tls.key }   # or a TLS proxy in front
//! auth:
//!   oidc:                           # sign in with the company's identity provider
//!     issuer: https://login.microsoftonline.com/<tenant>/v2.0
//!     client_id: 00000000-0000-0000-0000-000000000000
//!   # or several providers, each a button in the app:
//!   # providers:
//!   #   - { id: google, issuer: https://accounts.google.com, client_id_env: GOOGLE_ID, client_secret_env: GOOGLE_SECRET }
//!   roles:                          # who may sign in, and who may create workspaces
//!     owners: [alice@example.com]
//!     allowed_domains: [example.com]
//!     default_role: operator
//! ```
//!
//! Everything else has a default (see [`CollabConfig`]).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use graphrt_console::Role;
use graphrt_console::config::{AuditConfig, OidcConfig, RolesConfig, SecretsConfig};
use serde::Deserialize;

use super::error::{ServerError, ServerResult};

/// How the server is run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// One organization: who may sign in follows `auth.roles`, and only
    /// people the roles make owners create workspaces.
    #[default]
    SelfHosted,
    /// Many teams on one server (Graphrt's hosted service): anyone the
    /// identity provider signs in may create workspaces, within `limits`.
    Hosted,
}

/// The config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollabConfig {
    #[serde(default)]
    pub mode: Mode,
    /// Where people and apps reach the server, e.g. `https://collab.example.com`.
    /// It must be `https://` unless it is a loopback address.
    pub public_url: String,
    /// The address to listen on. Default: `127.0.0.1:8790`.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Where the server keeps its files.
    pub data_dir: PathBuf,
    /// Serve HTTPS directly. Without it, put a TLS proxy in front (see the guide).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub audit: AuditConfig,
    /// The key that encrypts workspace secrets (the console's key format).
    #[serde(default = "default_secrets")]
    pub secrets: SecretsConfig,
    /// Behind a reverse proxy: the client address is the last `X-Forwarded-For`
    /// entry (for rate limits). Only set it when a proxy always sets that header.
    #[serde(default)]
    pub behind_proxy: bool,
    /// Web origins whose pages may open sync sessions. The public URL's origin always may.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Plugin services (cargo feature `services`).
    #[serde(default)]
    pub plugins: PluginsConfig,
}

/// A certificate and key in PEM files.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// How people sign in.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// The identity provider. Without one (and without `providers`), people join with invite codes only.
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    /// Several identity providers, each a sign in button in the app (e.g.
    /// Google and Microsoft). `oidc` is the same as one provider with the id `oidc`.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Who may sign in with the provider, and (self hosted) who creates
    /// workspaces: people the roles make owners.
    #[serde(default)]
    pub roles: RolesConfig,
    /// Whether an invite code alone makes an account, for teams without an
    /// identity provider. Default: on without `oidc`, off with it.
    #[serde(default)]
    pub invite_accounts: Option<bool>,
    /// Minutes an access token lasts. Default: 10.
    #[serde(default = "default_access_minutes")]
    pub access_token_minutes: u64,
    /// Days a sign in lasts without use. Default: 30.
    #[serde(default = "default_refresh_days")]
    pub refresh_token_days: u64,
    /// The variable that holds the token signing key (32 bytes, base64 or hex).
    /// Without it the server keeps a generated key in `data_dir`.
    #[serde(default = "default_signing_key_env")]
    pub signing_key_env: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            oidc: None,
            providers: Vec::new(),
            roles: RolesConfig::default(),
            invite_accounts: None,
            access_token_minutes: default_access_minutes(),
            refresh_token_days: default_refresh_days(),
            signing_key_env: default_signing_key_env(),
        }
    }
}

impl AuthConfig {
    /// Whether invite codes make accounts.
    pub fn invite_accounts(&self) -> bool {
        self.invite_accounts.unwrap_or(self.oidc.is_none() && self.providers.is_empty())
    }

    /// Every identity provider: `oidc` (as the provider `oidc`) first, then `providers`.
    pub fn all_providers(&self) -> Vec<ProviderConfig> {
        let legacy = self.oidc.iter().map(|oidc| ProviderConfig {
            id: LEGACY_PROVIDER.to_owned(),
            name: None,
            issuer: oidc.issuer.clone(),
            client_id: Some(oidc.client_id.clone()),
            client_id_env: None,
            client_secret_env: oidc.client_secret_env.clone(),
            scopes: oidc.scopes.clone(),
            groups_claim: oidc.groups_claim.clone(),
            auth_params: oidc.auth_params.clone(),
            email_verified_claim: None,
        });
        legacy.chain(self.providers.iter().cloned()).collect()
    }
}

/// The id of the provider `auth.oidc` configures.
pub const LEGACY_PROVIDER: &str = "oidc";

/// One identity provider (OpenID Connect).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// A short id the app sends to pick this provider, e.g. `google` (a to z, 0 to 9, `-`).
    pub id: String,
    /// The name on the sign in button. Default: `Google` or `Microsoft` for
    /// their issuers, else the issuer's host.
    #[serde(default)]
    pub name: Option<String>,
    /// The issuer; `<issuer>/.well-known/openid-configuration` describes it.
    /// Microsoft's `https://login.microsoftonline.com/common/v2.0` signs in
    /// work, school and personal accounts of any tenant.
    pub issuer: String,
    /// The client id registered at the provider.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The environment variable that holds the client id, instead of `client_id`.
    /// While it is empty the provider is off (the server starts without it and says so).
    #[serde(default)]
    pub client_id_env: Option<String>,
    /// The environment variable that holds the client secret. Without one the
    /// server signs in as a public client (PKCE only).
    #[serde(default)]
    pub client_secret_env: Option<String>,
    /// Scopes asked for. Default: `openid email profile`.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// The ID token claim that lists the person's groups. Default: `groups`.
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    /// More parameters of the authorization request, e.g. `{prompt: select_account}`.
    #[serde(default)]
    pub auth_params: BTreeMap<String, String>,
    /// A claim that must be `true` for the email of the token to be used, for a
    /// provider that does not say `email_verified` (Microsoft: `xms_edov`). Without
    /// it, an email is used unless the token says `email_verified: false`.
    #[serde(default)]
    pub email_verified_claim: Option<String>,
}

impl ProviderConfig {
    /// The name on the sign in button.
    pub fn display_name(&self) -> String {
        if let Some(name) = self.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            return name.to_owned();
        }
        let host = self.issuer.split("://").nth(1).and_then(|rest| rest.split('/').next()).unwrap_or(&self.issuer);
        match host {
            "accounts.google.com" => "Google".to_owned(),
            "login.microsoftonline.com" => "Microsoft".to_owned(),
            other => other.to_owned(),
        }
    }

    /// The console's provider config, with the client id it resolved to.
    pub fn oidc(&self, client_id: String) -> OidcConfig {
        OidcConfig {
            issuer: self.issuer.clone(),
            client_id,
            client_secret_env: self.client_secret_env.clone(),
            scopes: self.scopes.clone(),
            groups_claim: self.groups_claim.clone(),
            auth_params: self.auth_params.clone(),
        }
    }

    fn check(&self) -> Result<(), String> {
        let id = &self.id;
        let id_ok =
            !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
        if !id_ok {
            return Err(format!("auth.providers: the id `{id}` is 1 to 32 of a to z, 0 to 9 and -"));
        }
        if self.issuer.trim().is_empty() {
            return Err(format!("auth.providers.{id} needs an `issuer`"));
        }
        let literal = self.client_id.as_deref().is_some_and(|c| !c.trim().is_empty());
        if literal == self.client_id_env.is_some() {
            return Err(format!("auth.providers.{id} needs one of `client_id` or `client_id_env`"));
        }
        if self.scopes.iter().all(|scope| scope != "openid") {
            return Err(format!("auth.providers.{id}: `scopes` must include openid"));
        }
        Ok(())
    }
}

fn default_scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(str::to_owned).to_vec()
}
fn default_groups_claim() -> String {
    "groups".to_owned()
}

/// Quotas and rates. The defaults suit a team server, a hosted server sets its plan's.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Workspaces one account may create (hosted). Default: 5.
    #[serde(default = "default_workspaces_per_account")]
    pub workspaces_per_account: usize,
    /// Projects in one workspace. Default: 200.
    #[serde(default = "default_projects")]
    pub projects_per_workspace: usize,
    /// Members of one workspace. Default: 100.
    #[serde(default = "default_members")]
    pub members_per_workspace: usize,
    /// The largest project document, encoded, in bytes. Default: 8 MiB.
    #[serde(default = "default_doc_bytes")]
    pub max_doc_bytes: usize,
    /// The largest sync message, in bytes. Default: 1 MiB.
    #[serde(default = "default_message_bytes")]
    pub max_message_bytes: usize,
    /// Open sync sessions of one account. Default: 20.
    #[serde(default = "default_sessions")]
    pub sessions_per_account: usize,
    /// Sync messages one session may send per second (bursts of twice that). Default: 60.
    #[serde(default = "default_messages_per_second")]
    pub messages_per_second: u32,
    /// HTTP requests one account (or one address, signed out) may make per minute. Default: 300.
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    /// Versions kept per project, besides named ones. Default: 100.
    #[serde(default = "default_versions")]
    pub auto_versions: usize,
    /// Minutes between automatic versions of a project being edited. Default: 10.
    #[serde(default = "default_version_minutes")]
    pub version_every_minutes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            workspaces_per_account: default_workspaces_per_account(),
            projects_per_workspace: default_projects(),
            members_per_workspace: default_members(),
            max_doc_bytes: default_doc_bytes(),
            max_message_bytes: default_message_bytes(),
            sessions_per_account: default_sessions(),
            messages_per_second: default_messages_per_second(),
            requests_per_minute: default_requests_per_minute(),
            auto_versions: default_versions(),
            version_every_minutes: default_version_minutes(),
        }
    }
}

/// Plugins whose services workspaces may enable.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Directories of installed plugins (each holds plugin directories).
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    /// Plugins allowed to receive the secrets a workspace grants them.
    #[serde(default)]
    pub trusted: Vec<String>,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8790))
}
fn default_secrets() -> SecretsConfig {
    SecretsConfig { key_env: "GRAPHRT_COLLAB_SECRETS_KEY".into(), key_file: None }
}
fn default_access_minutes() -> u64 {
    10
}
fn default_refresh_days() -> u64 {
    30
}
fn default_signing_key_env() -> String {
    "GRAPHRT_COLLAB_SIGNING_KEY".into()
}
fn default_workspaces_per_account() -> usize {
    5
}
fn default_projects() -> usize {
    200
}
fn default_members() -> usize {
    100
}
fn default_doc_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_message_bytes() -> usize {
    1024 * 1024
}
fn default_sessions() -> usize {
    20
}
fn default_messages_per_second() -> u32 {
    60
}
fn default_requests_per_minute() -> u32 {
    300
}
fn default_versions() -> usize {
    100
}
fn default_version_minutes() -> u64 {
    10
}

/// The parts of the public URL the server uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicUrl {
    /// `https://collab.example.com` (no trailing slash, no path).
    pub origin: String,
    /// A path prefix of a reverse proxy, e.g. `/collab` (empty without one).
    pub prefix: String,
}

impl PublicUrl {
    /// `origin + prefix + path`.
    pub fn join(&self, path: &str) -> String {
        format!("{}{}{path}", self.origin, self.prefix)
    }
}

impl CollabConfig {
    /// Read and check the config at `path`.
    ///
    /// @throws ServerError::Config - When the file cannot be read or is not a valid config.
    pub fn load(path: &Path) -> ServerResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| ServerError::Config(format!("cannot read {}: {error}", path.display())))?;
        let config: Self = graphrt_core::yaml::from_str(&text, true)
            .map_err(|error| ServerError::Config(format!("{}: {error}", path.display())))?;
        config.check()?;
        Ok(config)
    }

    /// Check the values (the loader calls it).
    ///
    /// @throws ServerError::Config - On the first value that is not usable.
    pub fn check(&self) -> ServerResult<()> {
        let invalid = |why: String| Err(ServerError::Config(why));
        self.public()?;
        if self.auth.access_token_minutes == 0 || self.auth.access_token_minutes > 60 {
            return invalid("auth.access_token_minutes is 1 to 60".into());
        }
        if self.auth.refresh_token_days == 0 || self.auth.refresh_token_days > 365 {
            return invalid("auth.refresh_token_days is 1 to 365".into());
        }
        let providers = self.auth.all_providers();
        if self.mode == Mode::Hosted && providers.is_empty() && !self.auth.invite_accounts() {
            return invalid(
                "a hosted server needs `auth.providers`, `auth.oidc` or `auth.invite_accounts: true`".into(),
            );
        }
        if let Some(oidc) = &self.auth.oidc
            && (oidc.issuer.trim().is_empty() || oidc.client_id.trim().is_empty())
        {
            return invalid("auth.oidc needs `issuer` and `client_id`".into());
        }
        for (at, provider) in providers.iter().enumerate() {
            provider.check().or_else(invalid)?;
            if providers.iter().take(at).any(|earlier| earlier.id == provider.id) {
                return invalid(format!("auth.providers: the id `{}` is used twice", provider.id));
            }
        }
        let limits = &self.limits;
        if limits.max_message_bytes < 1024 || limits.max_message_bytes > 16 * 1024 * 1024 {
            return invalid("limits.max_message_bytes is 1 KiB to 16 MiB".into());
        }
        if limits.max_doc_bytes < limits.max_message_bytes {
            return invalid("limits.max_doc_bytes is at least limits.max_message_bytes".into());
        }
        if limits.messages_per_second == 0 || limits.requests_per_minute == 0 {
            return invalid("limits.messages_per_second and limits.requests_per_minute are above 0".into());
        }
        for origin in &self.allowed_origins {
            if !(origin.starts_with("https://") || origin.starts_with("http://")) || origin.ends_with('/') {
                return invalid(format!("allowed_origins: `{origin}` is not an origin like https://app.example.com"));
            }
        }
        Ok(())
    }

    /// The public URL, checked.
    ///
    /// @throws ServerError::Config - When it is not an http(s) URL, or plain http to a non loopback host.
    pub fn public(&self) -> ServerResult<PublicUrl> {
        let url = self.public_url.trim().trim_end_matches('/');
        let (scheme, rest) =
            url.split_once("://").ok_or_else(|| ServerError::Config(format!("public_url `{url}` is not a URL")))?;
        let (host, path) = match rest.find('/') {
            Some(at) => (rest.get(..at).unwrap_or(rest), rest.get(at..).unwrap_or("")),
            None => (rest, ""),
        };
        if host.is_empty() || host.contains('@') {
            return Err(ServerError::Config(format!("public_url `{url}` has no host")));
        }
        let hostname = host.rsplit_once(':').map_or(host, |(name, _port)| name);
        let loopback = matches!(hostname, "localhost" | "127.0.0.1" | "[::1]");
        match scheme {
            "https" => {}
            "http" if loopback => {}
            "http" => {
                return Err(ServerError::Config(format!(
                    "public_url `{url}` must be https:// (people sign in and edit through it), plain http is only for loopback"
                )));
            }
            _ => return Err(ServerError::Config(format!("public_url `{url}` is not an http(s) URL"))),
        }
        Ok(PublicUrl { origin: format!("{scheme}://{host}"), prefix: path.to_owned() })
    }

    /// The role `identity` gets from `auth.roles` (self hosted), `None` for no access.
    pub fn server_role(&self, identity: &graphrt_console::roles::Identity) -> Option<Role> {
        graphrt_console::roles::mapped_role(&self.auth.roles, identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> ServerResult<CollabConfig> {
        let config: CollabConfig = graphrt_core::yaml::from_str(yaml, true).map_err(ServerError::Config)?;
        config.check()?;
        Ok(config)
    }

    #[test]
    fn minimal_config_has_defaults() {
        let c = config("public_url: http://127.0.0.1:8790\ndata_dir: /tmp/x\n").unwrap();
        assert_eq!(c.mode, Mode::SelfHosted);
        assert!(c.auth.invite_accounts());
        assert_eq!(c.auth.access_token_minutes, 10);
        assert_eq!(c.public().unwrap().origin, "http://127.0.0.1:8790");
        assert_eq!(c.limits.max_message_bytes, 1024 * 1024);
    }

    #[test]
    fn refuses_plain_http_off_loopback() {
        assert!(config("public_url: http://collab.example.com\ndata_dir: /tmp/x\n").is_err());
        let c = config("public_url: https://example.com/collab/\ndata_dir: /tmp/x\n").unwrap();
        let public = c.public().unwrap();
        assert_eq!(public.origin, "https://example.com");
        assert_eq!(public.prefix, "/collab");
        assert_eq!(public.join("/v1/x"), "https://example.com/collab/v1/x");
    }

    #[test]
    fn refuses_unknown_keys_and_bad_limits() {
        assert!(config("public_url: https://x.example\ndata_dir: /tmp/x\nmodee: hosted\n").is_err());
        assert!(config("public_url: https://x.example\ndata_dir: /tmp/x\nauth: {access_token_minutes: 0}\n").is_err());
        assert!(config("public_url: https://x.example\ndata_dir: /tmp/x\nlimits: {max_message_bytes: 10}\n").is_err());
        assert!(
            config("public_url: https://x.example\ndata_dir: /tmp/x\nallowed_origins: ['https://a.example/']\n")
                .is_err()
        );
    }

    #[test]
    fn invite_accounts_default_follows_oidc() {
        let c = config(
            "public_url: https://x.example\ndata_dir: /tmp/x\nmode: hosted\nauth:\n  oidc: {issuer: 'https://id.example', client_id: c}\n",
        )
        .unwrap();
        assert!(!c.auth.invite_accounts());
        assert_eq!(c.auth.all_providers().len(), 1);
        assert_eq!(c.auth.all_providers()[0].id, LEGACY_PROVIDER);
    }

    #[test]
    fn several_providers_are_checked() {
        let base = "public_url: https://x.example\ndata_dir: /tmp/x\nmode: hosted\nauth:\n  providers:\n";
        let google = "    - {id: google, issuer: 'https://accounts.google.com', client_id_env: G_ID, client_secret_env: G_SECRET}\n";
        let microsoft = "    - {id: microsoft, issuer: 'https://login.microsoftonline.com/common/v2.0', client_id: m, email_verified_claim: xms_edov}\n";
        let c = config(&format!("{base}{google}{microsoft}")).unwrap();
        assert!(!c.auth.invite_accounts());
        let names: Vec<String> = c.auth.all_providers().iter().map(ProviderConfig::display_name).collect();
        assert_eq!(names, ["Google", "Microsoft"]);
        // The same id twice, a bad id, both or neither client id, no openid scope.
        assert!(config(&format!("{base}{google}{google}")).is_err());
        assert!(config(&format!("{base}    - {{id: Google, issuer: 'https://a.example', client_id: c}}\n")).is_err());
        assert!(config(&format!("{base}    - {{id: a, issuer: 'https://a.example'}}\n")).is_err());
        assert!(
            config(&format!("{base}    - {{id: a, issuer: 'https://a.example', client_id: c, client_id_env: C}}\n"))
                .is_err()
        );
        assert!(
            config(&format!("{base}    - {{id: a, issuer: 'https://a.example', client_id: c, scopes: [email]}}\n"))
                .is_err()
        );
        let named = config(&format!("{base}    - {{id: a, name: Acme, issuer: 'https://a.example', client_id: c}}\n"));
        assert_eq!(named.unwrap().auth.all_providers()[0].display_name(), "Acme");
    }
}
