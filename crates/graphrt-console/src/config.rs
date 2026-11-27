//! The console config: one YAML (or JSON) file named by `graphrt serve --console`.
//!
//! ```yaml
//! public_url: https://agents.example.com     # where people open the console
//! environment: prod                          # what this server serves
//! environments: [dev, prod]                  # the order versions are promoted in
//! oidc:
//!   issuer: https://login.microsoftonline.com/<tenant>/v2.0
//!   client_id: 00000000-0000-0000-0000-000000000000
//!   client_secret_env: GRAPHRT_OIDC_CLIENT_SECRET
//!   groups_claim: groups
//! roles:
//!   owners: [alice@example.com]              # always owners, by email or subject
//!   groups: {graphrt-admins: owner, graphrt-ops: operator, staff: viewer}
//! ```
//!
//! Everything else has a default (see [`ConsoleConfig`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{ConsoleError, Result};
use crate::roles::Role;

/// The console config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleConfig {
    /// The address people open the console at, e.g. `https://agents.example.com`.
    /// The sign in callback is `<public_url>/console/callback`; its host is the
    /// one `Host` a signed in browser may use (DNS rebinding protection).
    pub public_url: String,
    /// More `Host` values a signed in browser may use (`name` or `name:port`).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Where the console keeps its files. Default: `console/` in the run store
    /// directory (`--store`).
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// The environment this server serves. Default: the last of `environments`.
    #[serde(default)]
    pub environment: Option<String>,
    /// Environments, in the order versions are promoted. Default: `[dev, prod]`.
    #[serde(default = "default_environments")]
    pub environments: Vec<String>,
    /// The OpenID Connect provider people sign in with.
    pub oidc: OidcConfig,
    /// Who gets which role.
    #[serde(default)]
    pub roles: RolesConfig,
    /// How long a sign in lasts.
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// The audit log.
    #[serde(default)]
    pub audit: AuditConfig,
    /// The key of the secrets.
    #[serde(default)]
    pub secrets: SecretsConfig,
}

/// The OpenID Connect provider (Entra ID, Google Workspace, Okta, Keycloak...).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// The issuer; `<issuer>/.well-known/openid-configuration` describes it.
    pub issuer: String,
    /// The client id registered at the provider.
    pub client_id: String,
    /// The environment variable that holds the client secret. Without one the
    /// console signs in as a public client (PKCE only).
    #[serde(default)]
    pub client_secret_env: Option<String>,
    /// Scopes asked for. Default: `openid email profile`.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// The ID token claim that lists the person's groups. Default: `groups`.
    /// Entra ID app roles are in `roles`.
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    /// More parameters of the authorization request, e.g. `{hd: example.com}`
    /// for Google Workspace or `{prompt: select_account}`.
    #[serde(default)]
    pub auth_params: BTreeMap<String, String>,
}

/// Who gets which role. The highest role that applies wins; a role an owner
/// set in the console replaces the mapped one, except for `owners`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    /// Always owners, by verified email or by subject. They cannot be demoted
    /// in the console: the way back in when every other owner is gone.
    #[serde(default)]
    pub owners: Vec<String>,
    /// Groups (values of the groups claim) and the role each gives.
    #[serde(default)]
    pub groups: BTreeMap<String, Role>,
    /// The role of a person no rule names. Default: none (they cannot sign in).
    #[serde(default)]
    pub default_role: Option<Role>,
    /// Email domains allowed to sign in (verified emails only). Default: any.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

/// How long a sign in lasts.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsConfig {
    /// Minutes without a request after which a session ends. Default: 60.
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
    /// Hours after which a session ends whatever happens. Default: 12.
    #[serde(default = "default_max_hours")]
    pub max_hours: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self { idle_minutes: default_idle_minutes(), max_hours: default_max_hours() }
    }
}

/// The audit log.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Days an entry is kept. Default: 365.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { retention_days: default_retention_days() }
    }
}

/// Where the key that encrypts secrets comes from: 32 bytes, as base64 or
/// hex, e.g. `openssl rand -base64 32`. Without a key, secrets cannot be set.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// The environment variable that holds the key. Default: `GRAPHRT_CONSOLE_KEY`.
    #[serde(default = "default_key_env")]
    pub key_env: String,
    /// A file that holds the key (a mounted secret), read when the variable is not set.
    #[serde(default)]
    pub key_file: Option<PathBuf>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { key_env: default_key_env(), key_file: None }
    }
}

fn default_environments() -> Vec<String> {
    vec!["dev".into(), "prod".into()]
}
fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "email".into(), "profile".into()]
}
fn default_groups_claim() -> String {
    "groups".into()
}
fn default_idle_minutes() -> u64 {
    60
}
fn default_max_hours() -> u64 {
    12
}
fn default_retention_days() -> u64 {
    365
}
fn default_key_env() -> String {
    "GRAPHRT_CONSOLE_KEY".into()
}

/// The parts of the public address the console checks requests against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicUrl {
    /// `https://agents.example.com` (no trailing slash).
    pub origin: String,
    /// A path prefix of a reverse proxy, e.g. `/graphrt` (empty without one).
    pub prefix: String,
    /// Whether the console is reached over HTTPS (cookies are then `Secure`).
    pub https: bool,
    /// `Host` values a signed in browser may send.
    pub hosts: Vec<String>,
}

impl ConsoleConfig {
    /// Read and check the config at `path`.
    ///
    /// @throws ConsoleError::Config - When the file cannot be read or is not a valid config.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| ConsoleError::Config(format!("cannot read {}: {error}", path.display())))?;
        let config: Self = graphrt_core::yaml::from_str(&text, true)
            .map_err(|error| ConsoleError::Config(format!("{}: {error}", path.display())))?;
        config.check()?;
        Ok(config)
    }

    /// Check the values (the loader calls it).
    ///
    /// @throws ConsoleError::Config - On the first value that is not usable.
    pub fn check(&self) -> Result<()> {
        let invalid = |why: String| Err(ConsoleError::Config(why));
        self.public()?;
        check_provider_url(&self.oidc.issuer).map_err(|why| ConsoleError::Config(format!("oidc.issuer: {why}")))?;
        if self.oidc.client_id.trim().is_empty() {
            return invalid("oidc.client_id is empty".into());
        }
        if !self.oidc.scopes.iter().any(|scope| scope == "openid") {
            return invalid("oidc.scopes must include `openid`".into());
        }
        if self.environments.is_empty() {
            return invalid("environments is empty".into());
        }
        for (index, name) in self.environments.iter().enumerate() {
            if !is_name(name) {
                return invalid(format!("environment `{name}`: letters, digits, `-` and `_`, up to 64"));
            }
            if self.environments.iter().take(index).any(|other| other == name) {
                return invalid(format!("environment `{name}` is listed twice"));
            }
        }
        if let Some(environment) = &self.environment
            && !self.environments.contains(environment)
        {
            return invalid(format!("environment `{environment}` is not one of environments"));
        }
        if self.sessions.idle_minutes == 0 || self.sessions.max_hours == 0 {
            return invalid("sessions.idle_minutes and sessions.max_hours must be at least 1".into());
        }
        if self.audit.retention_days == 0 {
            return invalid("audit.retention_days must be at least 1".into());
        }
        Ok(())
    }

    /// The environment this server serves.
    pub fn environment(&self) -> &str {
        self.environment.as_deref().or(self.environments.last().map(String::as_str)).unwrap_or("prod")
    }

    /// The public address, taken apart.
    ///
    /// @throws ConsoleError::Config - When `public_url` is not an `https` address (`http` only for loopback).
    pub fn public(&self) -> Result<PublicUrl> {
        let invalid = |why: &str| ConsoleError::Config(format!("public_url `{}`: {why}", self.public_url));
        let url = reqwest::Url::parse(self.public_url.trim()).map_err(|_| invalid("not an address"))?;
        let host = url.host_str().ok_or_else(|| invalid("no host"))?.to_ascii_lowercase();
        let https = match url.scheme() {
            "https" => true,
            "http" if is_loopback(&host) => false,
            _ => return Err(invalid("must be https (http only for localhost)")),
        };
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err(invalid("no user, query or fragment"));
        }
        let host_port = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let default_port = if https { 443 } else { 80 };
        let mut hosts = vec![host_port.clone()];
        if url.port().is_none() {
            hosts.push(format!("{host}:{default_port}"));
        }
        for extra in &self.allowed_hosts {
            hosts.push(extra.trim().to_ascii_lowercase());
        }
        let origin = format!("{}://{host_port}", url.scheme());
        let prefix = url.path().trim_end_matches('/').to_owned();
        Ok(PublicUrl { origin, prefix, https, hosts })
    }
}

/// Whether `name` is a valid environment, workflow or secret scope name.
pub(crate) fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.')
}

fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare == "localhost" || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// An identity provider address must be `https` (`http` only for loopback).
pub(crate) fn check_provider_url(url: &str) -> std::result::Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| format!("`{url}` is not an address"))?;
    let loopback = parsed.host_str().is_some_and(is_loopback);
    if parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(format!("`{url}` must be https (http only for localhost)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> std::result::Result<ConsoleConfig, String> {
        let text = format!(
            "public_url: https://agents.example.com\noidc:\n  issuer: https://idp.example.com\n  client_id: graphrt\n{extra}"
        );
        let config: ConsoleConfig = graphrt_core::yaml::from_str(&text, true)?;
        config.check().map_err(|error| error.to_string())?;
        Ok(config)
    }

    #[test]
    fn defaults_fill_a_minimal_config() -> std::result::Result<(), String> {
        let config = config("")?;
        assert_eq!(config.environment(), "prod");
        assert_eq!(config.oidc.scopes, ["openid", "email", "profile"]);
        assert_eq!(config.sessions.idle_minutes, 60);
        let public = config.public().map_err(|e| e.to_string())?;
        assert_eq!(public.origin, "https://agents.example.com");
        assert!(public.https);
        assert_eq!(public.hosts, ["agents.example.com", "agents.example.com:443"]);
        Ok(())
    }

    #[test]
    fn roles_map_groups() -> std::result::Result<(), String> {
        let config = config("roles:\n  owners: [a@example.com]\n  groups: {ops: operator, staff: viewer}\n")?;
        assert_eq!(config.roles.groups.get("ops"), Some(&Role::Operator));
        Ok(())
    }

    #[test]
    fn refuses_unsafe_or_inconsistent_values() {
        assert!(config("environment: staging\n").is_err());
        assert!(config("environments: [dev, dev]\n").is_err());
        assert!(config("unknown: 1\n").is_err());
        let http = "public_url: http://agents.example.com\noidc: {issuer: https://idp, client_id: x}\n";
        let parsed: ConsoleConfig = graphrt_core::yaml::from_str(http, true).unwrap_or_else(|_| unreachable!());
        assert!(parsed.check().is_err());
        let plain_idp = "public_url: https://a.example\noidc: {issuer: http://idp.example, client_id: x}\n";
        let parsed: ConsoleConfig = graphrt_core::yaml::from_str(plain_idp, true).unwrap_or_else(|_| unreachable!());
        assert!(parsed.check().is_err());
    }

    #[test]
    fn loopback_may_use_http_and_a_prefix() -> std::result::Result<(), String> {
        let text = "public_url: http://127.0.0.1:8080/graphrt/\noidc: {issuer: http://localhost:9000, client_id: x}\n";
        let config: ConsoleConfig = graphrt_core::yaml::from_str(text, true)?;
        config.check().map_err(|e| e.to_string())?;
        let public = config.public().map_err(|e| e.to_string())?;
        assert_eq!(
            (public.origin.as_str(), public.prefix.as_str(), public.https),
            ("http://127.0.0.1:8080", "/graphrt", false)
        );
        Ok(())
    }
}
