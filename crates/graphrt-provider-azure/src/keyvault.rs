//! Key Vault secret references.
//!
//! Container Apps resolves `configuration.secrets[].keyVaultUrl` itself with
//! the app's managed identity, so the gateway never holds a secret value: a
//! deployment spec carries only [`KeyVaultSecretRef`]s. At deploy time,
//! `graphrt deploy` may write a value once with [`KeyVaultClient::set_secret`]
//! (deploy-time credentials, `Key Vault Secrets Officer`).

use crate::http::json_at;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::api::{ProviderError, SecretString};
use crate::auth::{KEY_VAULT_SCOPE, TokenProvider};
use crate::http::{build_client, percent_encode, read_body};

/// Which vault hosts tokens may be sent to (operator configuration).
#[derive(Clone, Debug)]
pub struct KeyVaultConfig {
    /// DNS suffixes of allowed vaults (`.vault.azure.net`; sovereign clouds:
    /// `.vault.usgovcloudapi.net`, `.vault.azure.cn`).
    pub allowed_host_suffixes: Vec<String>,
    /// Only `https://` vault URLs; tests against a fake server turn this off.
    pub require_https: bool,
    /// Data-plane api-version.
    pub api_version: String,
}

impl Default for KeyVaultConfig {
    fn default() -> Self {
        Self { allowed_host_suffixes: vec![".vault.azure.net".into()], require_https: true, api_version: "7.4".into() }
    }
}

/// A reference to a Key Vault secret, as an app secret named `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyVaultSecretRef {
    /// App-level secret name (`[a-z0-9-]`, referenced by env `secretRef`).
    pub name: String,
    /// `https://<vault>.vault.azure.net/secrets/<secret>[/<version>]`;
    /// versionless = latest, which Container Apps re-reads within 30 minutes.
    pub secret_url: String,
    /// `"system"` or the resource id of a user-assigned identity with
    /// `Key Vault Secrets User` on the vault.
    pub identity: String,
}

impl KeyVaultSecretRef {
    /// The `configuration.secrets[]` entry of a container app.
    pub fn to_arm(&self) -> serde_json::Value {
        json!({ "name": self.name, "keyVaultUrl": self.secret_url, "identity": self.identity })
    }

    /// @throws `ProviderError::Fatal` for an invalid name, vault host or identity.
    pub fn validate(&self, config: &KeyVaultConfig) -> Result<(), ProviderError> {
        if !is_app_secret_name(&self.name) {
            return Err(ProviderError::Fatal(format!(
                "secret name {:?} must be 1-253 lowercase letters, digits, '-' or '.'",
                self.name
            )));
        }
        let (_vault_base, rest) = split_vault_url(&self.secret_url, config)?;
        let mut segments = rest.trim_end_matches('/').split('/');
        let valid_path = segments.next() == Some("secrets")
            && segments.next().is_some_and(is_vault_secret_name)
            && segments.next().is_none_or(|version| version.chars().all(|c| c.is_ascii_alphanumeric()))
            && segments.next().is_none();
        if !valid_path {
            return Err(ProviderError::Fatal(format!("{:?} is not a Key Vault secret URL", self.secret_url)));
        }
        if self.identity != "system"
            && !self
                .identity
                .to_ascii_lowercase()
                .contains("/providers/microsoft.managedidentity/userassignedidentities/")
        {
            return Err(ProviderError::Fatal(
                "Key Vault identity must be \"system\" or a user-assigned identity resource id".into(),
            ));
        }
        Ok(())
    }
}

/// Key Vault data-plane client (deploy time only).
#[derive(Debug, Clone)]
pub struct KeyVaultClient {
    client: reqwest::Client,
    tokens: Arc<TokenProvider>,
    config: KeyVaultConfig,
}

impl KeyVaultClient {
    /// @throws `ProviderError::Fatal` when the HTTP client cannot be built.
    pub fn new(config: KeyVaultConfig, tokens: Arc<TokenProvider>) -> Result<Self, ProviderError> {
        let client = build_client(Duration::from_secs(30)).map_err(ProviderError::Fatal)?;
        Ok(Self { client, tokens, config })
    }

    /// Store `value` as secret `name` in `vault_url` and return its versionless
    /// URL for a [`KeyVaultSecretRef`]. The value never appears in errors.
    ///
    /// @throws `ProviderError::Fatal` for a disallowed vault host or name, `Auth`
    ///   when the identity lacks `secrets/set`, `Transient` on server errors.
    pub async fn set_secret(&self, vault_url: &str, name: &str, value: &SecretString) -> Result<String, ProviderError> {
        let (vault_base, _) = split_vault_url(vault_url, &self.config)?;
        if !is_vault_secret_name(name) {
            return Err(ProviderError::Fatal(format!("{name:?} is not a valid Key Vault secret name")));
        }
        let secret_url = format!("{vault_base}/secrets/{}", percent_encode(name));
        let url = format!("{secret_url}?api-version={}", percent_encode(&self.config.api_version));
        let body = json!({ "value": value.expose() }).to_string();
        let mut refreshed = false;
        loop {
            let token = self.tokens.token(KEY_VAULT_SCOPE).await?;
            let response = self
                .client
                .put(&url)
                .bearer_auth(token.expose())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(|error| ProviderError::Transient(format!("Key Vault unreachable: {}", error.without_url())))?;
            let status = response.status().as_u16();
            let retry_after = crate::http::retry_after(response.headers());
            let bytes = read_body(response).await.map_err(ProviderError::Transient)?;
            if status == 401 && !refreshed {
                self.tokens.invalidate(KEY_VAULT_SCOPE, &token);
                refreshed = true;
                continue;
            }
            if (200..300).contains(&status) {
                return Ok(secret_url);
            }
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            let code = json_at(&parsed, "/error/code").as_str().unwrap_or("unknown").to_string();
            let summary = format!("Key Vault set secret {name:?}: HTTP {status} {code}");
            return Err(match status {
                401 | 403 => ProviderError::Auth(summary),
                429 => ProviderError::RateLimited { retry_after },
                500..=599 => ProviderError::Transient(summary),
                _ => ProviderError::Fatal(summary),
            });
        }
    }
}

/// `(scheme://host[:port], path after the host without the leading '/')`,
/// after checking the scheme and the host suffix.
fn split_vault_url<'a>(url: &'a str, config: &KeyVaultConfig) -> Result<(&'a str, &'a str), ProviderError> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| if config.require_https { None } else { url.strip_prefix("http://") })
        .ok_or_else(|| ProviderError::Fatal(format!("Key Vault URL {url:?} must use https")))?;
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = rest.get(..authority_end).unwrap_or("");
    let host = authority.rsplit_once(':').map_or(authority, |(host, _port)| host).to_ascii_lowercase();
    let allowed = config.allowed_host_suffixes.iter().any(|suffix| {
        let suffix = suffix.to_ascii_lowercase();
        host == suffix.trim_start_matches('.') || (suffix.starts_with('.') && host.ends_with(&suffix))
    });
    if host.is_empty() || !allowed || authority.contains('@') {
        return Err(ProviderError::Fatal(format!("Key Vault host {host:?} is not an allowed vault")));
    }
    let base_len = url.len() - rest.len() + authority_end;
    let base = url.get(..base_len).unwrap_or(url);
    let path = url.get(base_len..).unwrap_or("").trim_start_matches('/');
    Ok((base, path))
}

/// Key Vault object names: 1-127 of `[0-9a-zA-Z-]`.
fn is_vault_secret_name(name: &str) -> bool {
    (1..=127).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Container Apps secret names: lowercase alphanumerics, '-' and '.'.
fn is_app_secret_name(name: &str) -> bool {
    (1..=253).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(url: &str, identity: &str) -> KeyVaultSecretRef {
        KeyVaultSecretRef { name: "openai-key".into(), secret_url: url.into(), identity: identity.into() }
    }

    #[test]
    fn valid_references_render_the_container_apps_shape() {
        let secret = reference("https://myvault.vault.azure.net/secrets/openai-key", "system");
        assert!(secret.validate(&KeyVaultConfig::default()).is_ok());
        assert_eq!(
            secret.to_arm(),
            json!({"name": "openai-key", "keyVaultUrl": "https://myvault.vault.azure.net/secrets/openai-key", "identity": "system"})
        );
        let pinned = reference(
            "https://myvault.vault.azure.net/secrets/openai-key/0123abcd",
            "/subscriptions/s/resourceGroups/r/providers/Microsoft.ManagedIdentity/userAssignedIdentities/gw",
        );
        assert!(pinned.validate(&KeyVaultConfig::default()).is_ok());
    }

    #[test]
    fn foreign_hosts_and_bad_paths_are_rejected_so_tokens_cannot_leak() {
        let config = KeyVaultConfig::default();
        for url in [
            "http://myvault.vault.azure.net/secrets/a",
            "https://evil.example.com/secrets/a",
            "https://myvault.vault.azure.net.evil.com/secrets/a",
            "https://user@myvault.vault.azure.net/secrets/a",
            "https://169.254.169.254/secrets/a",
            "https://myvault.vault.azure.net/keys/a",
            "https://myvault.vault.azure.net/secrets/a/b/c",
        ] {
            assert!(reference(url, "system").validate(&config).is_err(), "{url} must be rejected");
        }
        assert!(reference("https://myvault.vault.azure.net/secrets/a", "someone-else").validate(&config).is_err());
    }
}
