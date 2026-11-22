//! Secret Manager writes of `graphrt deploy`: the CLI stores each secret value as
//! a new version and the Cloud Run spec only carries the reference
//! (`projects/p/secrets/s/versions/n`). The gateway never sees the values (C10).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use graphrt_provider_api::http::{ProviderHttp, SecretString, TokenBucket, TokenSource};
use graphrt_provider_api::{ProviderError, ProviderErrorKind, SecretRef};
use serde_json::Value;

use crate::auth::GcpAuth;
use crate::cloud_run::CloudRunConfig;
use crate::http_client::{ClientOptions, build_client};
use crate::rest::{Method, send_json};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Secret Manager client of one project (deploy-time: `roles/secretmanager.admin`,
/// or `secretVersionAdder` when the secrets already exist).
#[derive(Clone)]
pub struct SecretManager {
    http: ProviderHttp,
    tokens: Arc<dyn TokenSource>,
    endpoint: String,
    project: String,
}

impl std::fmt::Debug for SecretManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SecretManager").field("project", &self.project).finish_non_exhaustive()
    }
}

/// 409 `ALREADY_EXISTS` on create is success; it must not be retried as a conflict.
fn classify_secret_status(status: u16, body: &str) -> ProviderErrorKind {
    if status == 409 && body.contains("ALREADY_EXISTS") {
        return ProviderErrorKind::Fatal;
    }
    ProviderError::kind_for_status(status)
}

impl SecretManager {
    /// A client for `config.project` at `config.secret_manager_endpoint`.
    ///
    /// @throws `ProviderError` (fatal) on an invalid configuration.
    pub fn new(config: &CloudRunConfig, auth: &GcpAuth) -> Result<Self, ProviderError> {
        config.validate()?;
        let client = build_client(ClientOptions { request_timeout: Some(REQUEST_TIMEOUT), bypass_proxy: false })?;
        let http =
            ProviderHttp::new(client, Arc::new(TokenBucket::new(5.0, 10))).with_classifier(classify_secret_status);
        Ok(Self {
            http,
            tokens: auth.access_tokens(),
            endpoint: config.secret_manager_endpoint.trim_end_matches('/').to_owned(),
            project: config.project.clone(),
        })
    }

    /// Stores `value` as a new version of `secret_id` (creating the secret with
    /// automatic replication when missing) and returns the pinned version reference.
    ///
    /// @param secret_id - `[A-Za-z0-9_-]{1,255}`
    /// @returns `projects/{project}/secrets/{secret_id}/versions/{n}`
    /// @throws `ProviderError` when the id is invalid or the API fails. No error
    ///   message contains the value.
    pub async fn put_secret(&self, secret_id: &str, value: &SecretString) -> Result<String, ProviderError> {
        let valid = !secret_id.is_empty()
            && secret_id.len() <= 255
            && secret_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte));
        if !valid {
            return Err(ProviderError::fatal(format!("invalid Secret Manager secret id `{secret_id}`")));
        }
        self.ensure_secret(secret_id).await?;
        let url = format!("{}/v1/projects/{}/secrets/{secret_id}:addVersion", self.endpoint, self.project);
        let body = serde_json::json!({ "payload": { "data": STANDARD.encode(value.expose()) } }).to_string();
        let created = send_json(&self.http, self.tokens.as_ref(), Method::Post, &url, Some(&body)).await?;
        let name = created
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::transient("addVersion response has no name"))?;
        // The API names the version with the project *number*; keep the id the
        // operator configured so references stay readable and comparable.
        let version = name.rsplit('/').next().unwrap_or("latest");
        Ok(format!("projects/{}/secrets/{secret_id}/versions/{version}", self.project))
    }

    /// Stores every `(env, value)` as secret `{prefix}{env}` (lowercased, `_` → `-`)
    /// and returns the references for the deployment spec.
    ///
    /// @throws `ProviderError` on the first failure.
    pub async fn store_secrets(
        &self,
        prefix: &str,
        values: &[(String, SecretString)],
    ) -> Result<Vec<SecretRef>, ProviderError> {
        let mut references = Vec::with_capacity(values.len());
        for (env, value) in values {
            let secret_id = format!("{prefix}{}", env.to_ascii_lowercase().replace('_', "-"));
            let reference = self.put_secret(&secret_id, value).await?;
            references.push(SecretRef { env: env.clone(), reference });
        }
        Ok(references)
    }

    async fn ensure_secret(&self, secret_id: &str) -> Result<(), ProviderError> {
        let url = format!("{}/v1/projects/{}/secrets?secretId={secret_id}", self.endpoint, self.project);
        let body = serde_json::json!({ "replication": { "automatic": {} }, "labels": { "managed-by": "graphrt" } })
            .to_string();
        let result = send_json(&self.http, self.tokens.as_ref(), Method::Post, &url, Some(&body)).await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.message.starts_with("HTTP 409") => Ok(()),
            Err(error) => Err(error),
        }
    }
}
