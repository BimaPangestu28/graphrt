//! Kubeconfig exec credential plugins (`users[].user.exec`), e.g.
//! `aws eks get-token`, `gke-gcloud-auth-plugin`, `kubelogin`.
//!
//! The plugin runs non-interactively with a timeout; its `ExecCredential`
//! token is cached until `expirationTimestamp - REFRESH_MARGIN` by the shared
//! [`super::TokenCache`]. Client certificates returned by a plugin are
//! not supported (a TLS identity cannot be swapped on a live client): use the
//! kubeconfig's `client-certificate-data` instead.

use std::process::Stdio;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use graphrt_provider_api::{BoxFuture, ProviderError};

use super::{AccessToken, AuthError, SecretString, TokenFetcher, UNKNOWN_EXPIRY, parse_rfc3339};

/// How long a plugin may run before it is killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
const SUPPORTED_API_VERSIONS: [&str; 2] = ["client.authentication.k8s.io/v1", "client.authentication.k8s.io/v1beta1"];

/// An exec plugin as configured in a kubeconfig.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecConfig {
    pub api_version: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Cluster info passed in `KUBERNETES_EXEC_INFO` when `provideClusterInfo` is set.
    pub cluster_server: Option<String>,
}

/// Runs an exec plugin to obtain bearer tokens.
#[derive(Debug)]
pub struct ExecTokenFetcher {
    config: ExecConfig,
}

impl ExecTokenFetcher {
    /// Validate a plugin config.
    ///
    /// @throws AuthError::Exec - unknown `apiVersion` or empty command
    pub fn new(config: ExecConfig) -> Result<Self, AuthError> {
        if !SUPPORTED_API_VERSIONS.contains(&config.api_version.as_str()) {
            return Err(AuthError::Exec(format!("unsupported apiVersion {:?}", config.api_version)));
        }
        if config.command.is_empty() {
            return Err(AuthError::Exec("empty command".into()));
        }
        Ok(Self { config })
    }

    fn exec_info(&self) -> String {
        let mut spec = serde_json::Map::new();
        spec.insert("interactive".into(), false.into());
        if let Some(server) = &self.config.cluster_server {
            spec.insert("cluster".into(), serde_json::json!({ "server": server }));
        }
        serde_json::json!({ "apiVersion": self.config.api_version, "kind": "ExecCredential", "spec": spec }).to_string()
    }

    async fn run(&self) -> Result<AccessToken, AuthError> {
        let mut command = tokio::process::Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .envs(self.config.env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
            .env("KUBERNETES_EXEC_INFO", self.exec_info())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child =
            command.spawn().map_err(|error| AuthError::Exec(format!("spawn {:?}: {error}", self.config.command)))?;
        let output = tokio::time::timeout(EXEC_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| AuthError::Exec(format!("{:?} did not finish within {EXEC_TIMEOUT:?}", self.config.command)))?
            .map_err(|error| AuthError::Exec(format!("wait for {:?}: {error}", self.config.command)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let excerpt: String = stderr.chars().take(512).collect();
            return Err(AuthError::Exec(format!(
                "{:?} exited with {}: {}",
                self.config.command,
                output.status,
                excerpt.trim()
            )));
        }
        parse_exec_credential(&output.stdout)
    }
}

impl TokenFetcher for ExecTokenFetcher {
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken, ProviderError>> {
        Box::pin(async move { Ok(self.run().await?) })
    }
}

#[derive(Deserialize)]
struct ExecCredential {
    kind: Option<String>,
    status: Option<ExecCredentialStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecCredentialStatus {
    token: Option<String>,
    expiration_timestamp: Option<String>,
    client_certificate_data: Option<String>,
}

/// Parse the plugin's stdout (an `ExecCredential` object).
fn parse_exec_credential(stdout: &[u8]) -> Result<AccessToken, AuthError> {
    // The error never echoes stdout: it carries the credential.
    let credential: ExecCredential = serde_json::from_slice(stdout)
        .map_err(|_| AuthError::Exec("output is not an ExecCredential JSON object".into()))?;
    if credential.kind.as_deref().is_some_and(|kind| kind != "ExecCredential") {
        return Err(AuthError::Exec("output kind is not ExecCredential".into()));
    }
    let status = credential.status.ok_or_else(|| AuthError::Exec("ExecCredential without status".into()))?;
    let Some(token) = status.token.filter(|token| !token.is_empty()) else {
        let reason = if status.client_certificate_data.is_some() {
            "client certificates from exec plugins are not supported, use client-certificate-data"
        } else {
            "ExecCredential status has no token"
        };
        return Err(AuthError::Unsupported(reason.into()));
    };
    let expires_in = match status.expiration_timestamp {
        Some(timestamp) => parse_rfc3339(&timestamp)
            .ok_or_else(|| AuthError::Exec(format!("bad expirationTimestamp {timestamp:?}")))?
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
        None => UNKNOWN_EXPIRY,
    };
    Ok(AccessToken { value: SecretString::new(token), expires_in })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tokens_with_and_without_expiry() {
        let token = parse_exec_credential(
            br#"{"kind":"ExecCredential","apiVersion":"client.authentication.k8s.io/v1","status":{"token":"k8s-aws-v1.abc","expirationTimestamp":"2999-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(token.value.expose(), "k8s-aws-v1.abc");
        assert!(token.expires_in > Duration::from_secs(3600));
        // Already expired (clock skew): zero lifetime, re-fetched on next use.
        let expired =
            parse_exec_credential(br#"{"status":{"token":"t","expirationTimestamp":"2001-01-01T00:00:00Z"}}"#).unwrap();
        assert_eq!(expired.expires_in, Duration::ZERO);
        let forever = parse_exec_credential(br#"{"status":{"token":"t"}}"#).unwrap();
        assert_eq!(forever.expires_in, UNKNOWN_EXPIRY);
    }

    #[test]
    fn rejects_bad_output_without_echoing_it() {
        let error = parse_exec_credential(b"secret-token-in-plain-text").unwrap_err();
        assert!(!error.to_string().contains("secret-token"));
        assert!(matches!(
            parse_exec_credential(br#"{"status":{"clientCertificateData":"pem","clientKeyData":"pem"}}"#),
            Err(AuthError::Unsupported(_))
        ));
        assert!(parse_exec_credential(br#"{"kind":"Other","status":{"token":"t"}}"#).is_err());
        assert!(parse_exec_credential(br#"{"status":{"token":"t","expirationTimestamp":"soon"}}"#).is_err());
    }

    #[test]
    fn rejects_unknown_api_versions() {
        let config = ExecConfig {
            api_version: "client.authentication.k8s.io/v1alpha1".into(),
            command: "true".into(),
            args: vec![],
            env: vec![],
            cluster_server: None,
        };
        assert!(ExecTokenFetcher::new(config).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_the_plugin_with_exec_info() {
        // The plugin echoes the apiVersion it was given in KUBERNETES_EXEC_INFO back as the token.
        let script = r#"info="$KUBERNETES_EXEC_INFO"; case "$info" in *'"interactive":false'*) ;; *) exit 3;; esac; printf '{"kind":"ExecCredential","status":{"token":"%s"}}' "$GREETING""#;
        let source = ExecTokenFetcher::new(ExecConfig {
            api_version: "client.authentication.k8s.io/v1".into(),
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("GREETING".into(), "from-plugin".into())],
            cluster_server: Some("https://k8s".into()),
        })
        .unwrap();
        assert_eq!(source.fetch().await.unwrap().value.expose(), "from-plugin");
        let failing = ExecTokenFetcher::new(ExecConfig {
            api_version: "client.authentication.k8s.io/v1".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "echo nope >&2; exit 7".into()],
            env: vec![],
            cluster_server: None,
        })
        .unwrap();
        let error = failing.fetch().await.unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
    }
}
