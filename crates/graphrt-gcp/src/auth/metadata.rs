//! The GCE metadata server (Compute Engine, Cloud Run, GKE workload identity):
//! access tokens of the attached service account and ID tokens for an audience.

use std::time::Duration;

use graphrt_provider_api::ProviderError;
use graphrt_provider_api::http::{AccessToken, SecretString};
use serde_json::Value;

use super::Grant;
use super::jwt::id_token_lifetime_secs;
use crate::url_encoding::encode_component;

const SERVICE_ACCOUNT_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default";

/// Fetches tokens from one metadata server base URL (`http://metadata.google.internal`).
#[derive(Debug, Clone)]
pub(crate) struct MetadataServer {
    base_url: String,
    http: reqwest::Client,
}

impl MetadataServer {
    pub(crate) fn new(base_url: String, http: reqwest::Client) -> Self {
        Self { base_url: base_url.trim_end_matches('/').to_owned(), http }
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetches a token for `grant`. The server must answer with
    /// `Metadata-Flavor: Google`; anything else is not a metadata server.
    pub(crate) async fn fetch(&self, grant: &Grant) -> Result<AccessToken, ProviderError> {
        let url = match grant {
            Grant::Access { scopes } if scopes.is_empty() => format!("{}{SERVICE_ACCOUNT_PATH}/token", self.base_url),
            Grant::Access { scopes } => {
                format!("{}{SERVICE_ACCOUNT_PATH}/token?scopes={}", self.base_url, encode_component(&scopes.join(",")))
            }
            Grant::Identity { audience } => format!(
                "{}{SERVICE_ACCOUNT_PATH}/identity?audience={}&format=full",
                self.base_url,
                encode_component(audience)
            ),
        };
        let response = self.http.get(url).header("Metadata-Flavor", "Google").send().await.map_err(|error| {
            ProviderError::transient(format!(
                "metadata server unreachable ({}); outside Google Cloud set GOOGLE_APPLICATION_CREDENTIALS",
                if error.is_timeout() { "timeout" } else { "connect error" }
            ))
        })?;
        let status = response.status().as_u16();
        let flavor_ok = response.headers().get("metadata-flavor").is_some_and(|value| value.as_bytes() == b"Google");
        if !flavor_ok {
            return Err(ProviderError::auth("response is not from a GCE metadata server (no Metadata-Flavor: Google)"));
        }
        if !(200..300).contains(&status) {
            let message = format!("metadata server HTTP {status}");
            return Err(match status {
                404 => ProviderError::auth(format!("{message}: no service account attached to this instance")),
                429 => ProviderError::rate_limited(message),
                500..=599 => ProviderError::transient(message),
                _ => ProviderError::auth(message),
            });
        }
        let body =
            response.text().await.map_err(|_| ProviderError::transient("metadata server response was cut off"))?;
        parse_metadata_response(grant, &body)
    }
}

fn parse_metadata_response(grant: &Grant, body: &str) -> Result<AccessToken, ProviderError> {
    match grant {
        Grant::Access { .. } => {
            let parsed: Value = serde_json::from_str(body)
                .map_err(|_| ProviderError::auth("metadata server token response is not JSON"))?;
            let token = parsed
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::auth("metadata server token response has no access_token"))?;
            let expires_in = parsed.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
            Ok(AccessToken { value: SecretString::new(token), expires_in: Duration::from_secs(expires_in) })
        }
        Grant::Identity { .. } => {
            let token = body.trim();
            if token.split('.').count() != 3 {
                return Err(ProviderError::auth("metadata server identity response is not a JWT"));
            }
            let lifetime = id_token_lifetime_secs(token);
            Ok(AccessToken { value: SecretString::new(token), expires_in: Duration::from_secs(lifetime) })
        }
    }
}
