//! Minimal hand-rolled client of the Fly Machines REST API (v1).
//!
//! Requests go through the shared provider HTTP layer
//! ([`graphrt_provider_api::http::ProviderHttp`]: token bucket, jittered retry
//! of 429/5xx/network errors) with the Fly classifier. Authentication is added
//! here because Fly macaroons use the `FlyV1` scheme, not `Bearer`; after a 401
//! the token is reloaded from its origin once and the call retried (C8).
//!
//! Every call the provider makes is idempotent by construction (deterministic
//! Machine names, start/suspend/stop of a known id), so retrying POSTs is safe.
//! Neither the token nor request bodies are ever logged or put in errors.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use graphrt_provider_api::http::{ProviderHttp, RetryPolicy, TokenBucket};
use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;

use crate::error::{classify_fly_response, decode_error};
use crate::model::{
    CreateAppRequest, CreateMachineRequest, ListSecretsResponse, Machine, MachineState, StopMachineRequest,
    UpdateSecretsRequest,
};
use crate::token::{FlyToken, TokenOrigin};

/// Public Machines API endpoint.
pub const PUBLIC_API_BASE_URL: &str = "https://api.machines.dev";
/// Endpoint reachable from inside a Fly organization (over 6PN, no TLS needed).
pub const INTERNAL_API_BASE_URL: &str = "http://_api.internal:4280";
/// Longest `timeout` the `/wait` endpoint accepts.
pub const MAX_WAIT_SECS: u64 = 60;

/// Rate limit and retry settings of the client.
#[derive(Clone, Debug)]
pub struct ClientPolicy {
    /// Retries of 429/5xx/network errors.
    pub retry: RetryPolicy,
    /// Sustained calls per second across the client. Fly limits mutations to
    /// about 1/s per Machine and action (burst 3); 20/s across the app keeps a
    /// wake storm of a pool well under the org-wide limits.
    pub requests_per_second: f64,
    /// Calls allowed in a burst.
    pub burst: u32,
    /// Timeout of one HTTP request (`/wait` holds up to 60 s).
    pub request_timeout: Duration,
}

impl Default for ClientPolicy {
    fn default() -> Self {
        Self {
            retry: RetryPolicy::default(),
            requests_per_second: 20.0,
            burst: 40,
            request_timeout: Duration::from_secs(MAX_WAIT_SECS + 15),
        }
    }
}

/// Client of one Fly app's Machines.
#[derive(Debug)]
pub struct MachinesClient {
    http: ProviderHttp,
    api_base_url: String,
    app_name: String,
    token_origin: TokenOrigin,
    current_token: Mutex<Option<FlyToken>>,
}

impl MachinesClient {
    /// Builds a client for `app_name`.
    ///
    /// @param api_base_url - [`PUBLIC_API_BASE_URL`] or [`INTERNAL_API_BASE_URL`]; from operator config, never from a deployment spec (String)
    /// @param app_name - Fly app holding the pool (String)
    /// @param token_origin - where the app's deploy token is read from (TokenOrigin)
    /// @param policy - retries and rate limit (ClientPolicy)
    /// @returns The client
    /// @throws ProviderError(Fatal) - invalid app name or a base URL that is not http(s)
    pub fn new(
        api_base_url: String,
        app_name: String,
        token_origin: TokenOrigin,
        policy: ClientPolicy,
    ) -> Result<Self, ProviderError> {
        validate_app_name(&app_name)?;
        if !(api_base_url.starts_with("https://") || api_base_url.starts_with("http://")) {
            return Err(ProviderError::fatal(format!("Fly Machines API URL must be http(s): {api_base_url:?}")));
        }
        let reqwest_client = reqwest::Client::builder()
            .timeout(policy.request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .http1_only()
            .build()
            .map_err(|error| ProviderError::fatal(format!("building the HTTP client: {error}")))?;
        let bucket = Arc::new(TokenBucket::new(policy.requests_per_second, policy.burst));
        let http =
            ProviderHttp::new(reqwest_client, bucket).with_policy(policy.retry).with_classifier(classify_fly_response);
        Ok(Self {
            http,
            api_base_url: api_base_url.trim_end_matches('/').to_owned(),
            app_name,
            token_origin,
            current_token: Mutex::new(None),
        })
    }

    /// Name of the app this client manages.
    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    /// `GET /apps/{app}/machines`, filtered server-side by metadata.
    ///
    /// @param metadata_filter - `metadata.{key}=value` pairs (&BTreeMap<String, String>)
    /// @returns Machines of the app (callers re-check metadata: the filter is a hint)
    pub async fn list_machines(
        &self,
        metadata_filter: &BTreeMap<String, String>,
    ) -> Result<Vec<Machine>, ProviderError> {
        let query: Vec<(String, String)> =
            metadata_filter.iter().map(|(key, value)| (format!("metadata.{key}"), value.clone())).collect();
        let body = self.send(reqwest::Method::GET, &self.machines_path(), &query, None).await?;
        serde_json::from_slice(&body).map_err(|error| decode_error("machine list", &error))
    }

    /// `GET /apps/{app}/machines/{id}`.
    pub async fn get_machine(&self, machine_id: &str) -> Result<Machine, ProviderError> {
        let body = self.send(reqwest::Method::GET, &self.machine_path(machine_id, "")?, &[], None).await?;
        serde_json::from_slice(&body).map_err(|error| decode_error("machine", &error))
    }

    /// `POST /apps/{app}/machines`.
    pub async fn create_machine(&self, request: &CreateMachineRequest) -> Result<Machine, ProviderError> {
        let body = self.send(reqwest::Method::POST, &self.machines_path(), &[], Some(encode(request)?)).await?;
        serde_json::from_slice(&body).map_err(|error| decode_error("created machine", &error))
    }

    /// `POST /apps/{app}/machines/{id}/start`: cold boot, or resume of a suspended snapshot.
    pub async fn start_machine(&self, machine_id: &str) -> Result<(), ProviderError> {
        self.send(reqwest::Method::POST, &self.machine_path(machine_id, "/start")?, &[], None).await.map(drop)
    }

    /// `POST /apps/{app}/machines/{id}/suspend`: snapshot memory, then stop.
    pub async fn suspend_machine(&self, machine_id: &str) -> Result<(), ProviderError> {
        self.send(reqwest::Method::POST, &self.machine_path(machine_id, "/suspend")?, &[], None).await.map(drop)
    }

    /// `POST /apps/{app}/machines/{id}/stop` with a signal and grace.
    pub async fn stop_machine(&self, machine_id: &str, request: &StopMachineRequest) -> Result<(), ProviderError> {
        self.send(reqwest::Method::POST, &self.machine_path(machine_id, "/stop")?, &[], Some(encode(request)?))
            .await
            .map(drop)
    }

    /// `GET /apps/{app}/machines/{id}/wait?state=..&timeout=..` (HTTP 408 when not reached, retried).
    ///
    /// @param machine_id - Machine to wait on (&str)
    /// @param state - target state (MachineState)
    /// @param timeout - capped at [`MAX_WAIT_SECS`] (Duration)
    pub async fn wait_machine(
        &self,
        machine_id: &str,
        state: MachineState,
        timeout: Duration,
    ) -> Result<(), ProviderError> {
        let query = [
            ("state".to_owned(), state.as_api_str().to_owned()),
            ("timeout".to_owned(), wait_timeout_secs(timeout).to_string()),
        ];
        self.send(reqwest::Method::GET, &self.machine_path(machine_id, "/wait")?, &query, None).await.map(drop)
    }

    /// `DELETE /apps/{app}/machines/{id}?force=true`.
    pub async fn destroy_machine(&self, machine_id: &str) -> Result<(), ProviderError> {
        let query = [("force".to_owned(), "true".to_owned())];
        self.send(reqwest::Method::DELETE, &self.machine_path(machine_id, "")?, &query, None).await.map(drop)
    }

    /// `GET /apps/{app}`: whether the app exists.
    pub async fn app_exists(&self) -> Result<bool, ProviderError> {
        let path = format!("/v1/apps/{}", self.app_name);
        match self.send(reqwest::Method::GET, &path, &[], None).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// `POST /apps`: creates this client's app in `org_slug`.
    pub async fn create_app(&self, org_slug: &str, network: &str) -> Result<(), ProviderError> {
        let request = CreateAppRequest {
            app_name: self.app_name.clone(),
            org_slug: org_slug.to_owned(),
            network: network.to_owned(),
        };
        self.send(reqwest::Method::POST, "/v1/apps", &[], Some(encode(&request)?)).await.map(drop)
    }

    /// `POST /apps/{app}/secrets`: sets (or, with `None`, deletes) app secrets in
    /// one call. Fly injects them as environment variables into Machines on boot.
    pub async fn update_secrets(&self, values: BTreeMap<&str, Option<&str>>) -> Result<(), ProviderError> {
        let path = format!("/v1/apps/{}/secrets", self.app_name);
        self.send(reqwest::Method::POST, &path, &[], Some(encode(&UpdateSecretsRequest { values })?)).await.map(drop)
    }

    /// `GET /apps/{app}/secrets`: names of the app's secrets (values are never requested).
    pub async fn list_secret_names(&self) -> Result<Vec<String>, ProviderError> {
        let path = format!("/v1/apps/{}/secrets", self.app_name);
        let body = self.send(reqwest::Method::GET, &path, &[], None).await?;
        let listing: ListSecretsResponse =
            serde_json::from_slice(&body).map_err(|error| decode_error("secret list", &error))?;
        Ok(listing.secrets.into_iter().map(|secret| secret.name).collect())
    }

    fn machines_path(&self) -> String {
        format!("/v1/apps/{}/machines", self.app_name)
    }

    fn machine_path(&self, machine_id: &str, suffix: &str) -> Result<String, ProviderError> {
        if machine_id.is_empty() || !machine_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(ProviderError::fatal(format!("invalid Fly Machine id {machine_id:?}")));
        }
        Ok(format!("/v1/apps/{}/machines/{machine_id}{suffix}", self.app_name))
    }

    /// Sends one call; on 401 reloads the token once and tries again.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        encoded_body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, ProviderError> {
        let mut reloaded = false;
        loop {
            let token = self.token(reloaded)?;
            let url = format!("{}{path}{}", self.api_base_url, encode_query(query));
            let build = |client: &reqwest::Client| {
                let mut request =
                    client.request(method.clone(), &url).header(AUTHORIZATION, token.authorization_header());
                if let Some(encoded_body) = &encoded_body {
                    request = request.header(CONTENT_TYPE, "application/json").body(encoded_body.clone());
                }
                request
            };
            match self.http.send(build, None).await {
                Ok(response) => {
                    let body = response
                        .bytes()
                        .await
                        .map_err(|error| ProviderError::transient(format!("reading response of {path}: {error}")))?;
                    return Ok(body.to_vec());
                }
                Err(error) if error.kind == ProviderErrorKind::Auth && !reloaded => reloaded = true,
                Err(error) => {
                    return Err(ProviderError { message: format!("Fly {method} {path}: {}", error.message), ..error });
                }
            }
        }
    }

    fn token(&self, force_reload: bool) -> Result<FlyToken, ProviderError> {
        let mut current_token = self.current_token.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = current_token.as_ref().filter(|_| !force_reload) {
            return Ok(token.clone());
        }
        let token = self.token_origin.load().map_err(ProviderError::auth)?;
        *current_token = Some(token.clone());
        Ok(token)
    }
}

fn validate_app_name(app_name: &str) -> Result<(), ProviderError> {
    let valid = !app_name.is_empty()
        && app_name.len() <= 63
        && app_name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid { Ok(()) } else { Err(ProviderError::fatal(format!("invalid Fly app name {app_name:?}"))) }
}

/// `?k=v&...` with RFC 3986 percent-encoding (reqwest's `query` feature is not linked).
fn encode_query(query: &[(String, String)]) -> String {
    let pairs: Vec<String> =
        query.iter().map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value))).collect();
    if pairs.is_empty() { String::new() } else { format!("?{}", pairs.join("&")) }
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn wait_timeout_secs(timeout: Duration) -> u64 {
    timeout.as_secs().clamp(1, MAX_WAIT_SECS)
}

fn encode(body: &impl Serialize) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(body).map_err(|error| ProviderError::fatal(format!("encoding a Fly request: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_app_names_and_urls() {
        let origin = TokenOrigin::Fixed(FlyToken::parse("fm2_x").unwrap());
        assert!(
            MachinesClient::new(PUBLIC_API_BASE_URL.into(), "a/../b".into(), origin.clone(), ClientPolicy::default())
                .is_err()
        );
        assert!(
            MachinesClient::new(PUBLIC_API_BASE_URL.into(), "Upper".into(), origin.clone(), ClientPolicy::default())
                .is_err()
        );
        assert!(MachinesClient::new("file:///etc".into(), "app".into(), origin, ClientPolicy::default()).is_err());
    }

    #[test]
    fn query_is_percent_encoded() {
        let query = [("metadata.graphrt_version".to_owned(), "v1 &x=y".to_owned())];
        assert_eq!(encode_query(&query), "?metadata.graphrt_version=v1%20%26x%3Dy");
        assert_eq!(encode_query(&[]), "");
    }

    #[test]
    fn wait_timeout_is_clamped_to_the_api_limit() {
        assert_eq!(wait_timeout_secs(Duration::from_secs(600)), 60);
        assert_eq!(wait_timeout_secs(Duration::from_millis(10)), 1);
    }
}
