//! The credential chain: env → container endpoint → web identity → IMDSv2.

use std::fmt;
use std::time::Duration;

use tokio::sync::Mutex;

use super::sigv4::AwsCredentials;
use super::sts::{self, Expiring, WebIdentityRequest};
use super::{AuthError, Clock};

/// Credentials are refreshed this long before they expire.
const REFRESH_BEFORE_EXPIRY_SECS: u64 = 300;
/// Largest credential / metadata response that is read.
const MAX_METADATA_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_CONTAINER_HOST: &str = "http://169.254.170.2";
const DEFAULT_IMDS_ENDPOINT: &str = "http://169.254.169.254";
const IMDS_TOKEN_TTL_SECS: &str = "21600";

/// Where the chain found credentials (for logs and diagnostics; never the secret itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Static,
    Environment,
    ContainerEndpoint,
    WebIdentity,
    Imds,
}

/// Inputs of the chain, normally read from the process environment.
///
/// Endpoints come from the gateway operator's environment, never from a
/// deployment spec, so a tenant cannot point the chain at an arbitrary host.
#[derive(Clone, Default)]
pub struct ChainConfig {
    pub static_credentials: Option<AwsCredentials>,
    pub container_relative_uri: Option<String>,
    pub container_full_uri: Option<String>,
    pub container_authorization_token: Option<String>,
    pub container_authorization_token_file: Option<String>,
    pub web_identity_token_file: Option<String>,
    pub role_arn: Option<String>,
    pub role_session_name: Option<String>,
    /// Region used for the regional STS endpoint.
    pub region: String,
    /// STS base URL override (VPC endpoint, tests).
    pub sts_endpoint: Option<String>,
    /// Base for `container_relative_uri` (default `http://169.254.170.2`; tests override it).
    pub container_host: Option<String>,
    pub imds_endpoint: Option<String>,
    pub imds_disabled: bool,
}

impl fmt::Debug for ChainConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainConfig")
            .field("static_credentials", &self.static_credentials)
            .field("container_relative_uri", &self.container_relative_uri)
            .field("container_full_uri", &self.container_full_uri)
            .field("container_authorization_token", &self.container_authorization_token.as_ref().map(|_| "<redacted>"))
            .field("container_authorization_token_file", &self.container_authorization_token_file)
            .field("web_identity_token_file", &self.web_identity_token_file)
            .field("role_arn", &self.role_arn)
            .field("region", &self.region)
            .field("sts_endpoint", &self.sts_endpoint)
            .field("imds_endpoint", &self.imds_endpoint)
            .field("imds_disabled", &self.imds_disabled)
            .finish_non_exhaustive()
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

impl ChainConfig {
    /// Build from an environment lookup (`std::env::var` in production).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let var = |name: &str| non_empty(lookup(name));
        let static_credentials = match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => {
                Some(AwsCredentials { access_key_id, secret_access_key, session_token: var("AWS_SESSION_TOKEN") })
            }
            _ => None,
        };
        Self {
            static_credentials,
            container_relative_uri: var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
            container_full_uri: var("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
            container_authorization_token: var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
            container_authorization_token_file: var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"),
            web_identity_token_file: var("AWS_WEB_IDENTITY_TOKEN_FILE"),
            role_arn: var("AWS_ROLE_ARN"),
            role_session_name: var("AWS_ROLE_SESSION_NAME"),
            region: var("AWS_REGION").or_else(|| var("AWS_DEFAULT_REGION")).unwrap_or_else(|| "us-east-1".into()),
            sts_endpoint: var("GRAPHRT_AWS_STS_ENDPOINT"),
            container_host: None,
            imds_endpoint: var("AWS_EC2_METADATA_SERVICE_ENDPOINT"),
            imds_disabled: var("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        }
    }

    /// Build from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

#[derive(Clone)]
struct Cached {
    credentials: AwsCredentials,
    expires_at: Option<u64>,
    source: CredentialSource,
}

/// Resolves and caches credentials. One refresh runs at a time (singleflight).
pub struct CredentialChain {
    config: ChainConfig,
    clock: Clock,
    /// Proxy-free client for link-local metadata endpoints and STS.
    http: reqwest::Client,
    cache: Mutex<Option<Cached>>,
}

impl fmt::Debug for CredentialChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialChain").field("config", &self.config).finish_non_exhaustive()
    }
}

/// Whether a full container credential URI may be called over plain HTTP: only
/// loopback and the documented ECS / EKS Pod Identity link-local addresses.
fn is_allowed_container_uri(uri: &str) -> bool {
    if uri.starts_with("https://") {
        return true;
    }
    let Some(rest) = uri.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority.split(']').next().map(|h| h.trim_start_matches('[')).unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host == "localhost"
        || host.starts_with("127.")
        || host == "::1"
        || host == "169.254.170.2"
        || host == "169.254.170.23"
        || host.eq_ignore_ascii_case("fd00:ec2::23")
}

fn fetch_error(source: &'static str, error: &reqwest::Error) -> AuthError {
    // reqwest errors carry the URL but never headers or bodies.
    AuthError::new(source, format!("request failed: {error}"))
}

async fn read_body(source: &'static str, response: reqwest::Response) -> Result<(u16, String), AuthError> {
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|e| fetch_error(source, &e))?;
    let bytes = bytes.get(..bytes.len().min(MAX_METADATA_BODY_BYTES)).unwrap_or_default();
    Ok((status, String::from_utf8_lossy(bytes).into_owned()))
}

/// Parse the JSON shape shared by the container endpoint and IMDS.
fn parse_json_credentials(source: &'static str, body: &str) -> Result<Expiring, AuthError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| AuthError::new(source, "credential response is not JSON"))?;
    if let Some(code) = value.get("Code").and_then(|c| c.as_str())
        && code != "Success"
    {
        return Err(AuthError::new(source, format!("credential endpoint returned Code={code}")));
    }
    let field = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| AuthError::new(source, format!("credential response has no {name}")))
    };
    let credentials = AwsCredentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: field("Token").ok(),
    };
    let expires_at = value.get("Expiration").and_then(|v| v.as_str()).and_then(sts::parse_iso8601);
    Ok((credentials, expires_at))
}

impl CredentialChain {
    /// A chain over `config`.
    ///
    /// @throws `AuthError` when the HTTP client cannot be built.
    pub fn new(config: ChainConfig, clock: Clock) -> Result<Self, AuthError> {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AuthError::new("chain", format!("cannot build HTTP client: {e}")))?;
        Ok(Self { config, clock, http, cache: Mutex::new(None) })
    }

    /// The clock shared with the signer.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Valid credentials, refreshed when within 5 minutes of expiry.
    ///
    /// @throws `AuthError` when no source in the chain yields credentials.
    pub async fn credentials(&self) -> Result<AwsCredentials, AuthError> {
        let mut cache = self.cache.lock().await;
        let now = self.clock.now_unix();
        if let Some(cached) = cache.as_ref()
            && cached.expires_at.is_none_or(|exp| now + REFRESH_BEFORE_EXPIRY_SECS < exp)
        {
            return Ok(cached.credentials.clone());
        }
        let (credentials, expires_at, source) = self.resolve().await?;
        tracing::debug!(?source, expires_at, "aws credentials refreshed");
        *cache = Some(Cached { credentials: credentials.clone(), expires_at, source });
        Ok(credentials)
    }

    /// Source of the cached credentials, if any.
    pub async fn source(&self) -> Option<CredentialSource> {
        self.cache.lock().await.as_ref().map(|c| c.source)
    }

    /// Drop cached credentials (after a 401/403) so the next call refetches.
    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    async fn resolve(&self) -> Result<(AwsCredentials, Option<u64>, CredentialSource), AuthError> {
        if let Some(static_credentials) = &self.config.static_credentials {
            return Ok((static_credentials.clone(), None, CredentialSource::Environment));
        }
        if self.config.container_relative_uri.is_some() || self.config.container_full_uri.is_some() {
            let (c, e) = self.fetch_container().await?;
            return Ok((c, e, CredentialSource::ContainerEndpoint));
        }
        if let (Some(file), Some(role)) = (&self.config.web_identity_token_file, &self.config.role_arn) {
            let (c, e) = self.fetch_web_identity(file, role).await?;
            return Ok((c, e, CredentialSource::WebIdentity));
        }
        if self.config.imds_disabled {
            return Err(AuthError::new(
                "chain",
                "no credentials: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, run with a task/pod role, \
                 or configure web identity (IMDS is disabled)",
            ));
        }
        let (c, e) = self.fetch_imds().await?;
        Ok((c, e, CredentialSource::Imds))
    }

    async fn container_token(&self) -> Result<Option<String>, AuthError> {
        if let Some(file) = &self.config.container_authorization_token_file {
            let token = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| AuthError::new("container", format!("cannot read token file {file}: {e}")))?;
            return Ok(Some(token.trim().to_owned()));
        }
        Ok(self.config.container_authorization_token.clone())
    }

    async fn fetch_container(&self) -> Result<Expiring, AuthError> {
        let url = match (&self.config.container_relative_uri, &self.config.container_full_uri) {
            (Some(relative), _) => {
                format!("{}{relative}", self.config.container_host.as_deref().unwrap_or(DEFAULT_CONTAINER_HOST))
            }
            (None, Some(full)) if is_allowed_container_uri(full) => full.clone(),
            (None, Some(_)) => {
                return Err(AuthError::new(
                    "container",
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI must be https or a loopback / ECS / EKS Pod Identity address",
                ));
            }
            (None, None) => return Err(AuthError::new("container", "no container credential URI")),
        };
        let mut request = self.http.get(&url);
        if let Some(token) = self.container_token().await? {
            request = request.header("authorization", token);
        }
        let response = request.send().await.map_err(|e| fetch_error("container", &e))?;
        let (status, body) = read_body("container", response).await?;
        if status != 200 {
            return Err(AuthError::new("container", format!("credential endpoint returned HTTP {status}")));
        }
        parse_json_credentials("container", &body)
    }

    async fn fetch_web_identity(&self, token_file: &str, role_arn: &str) -> Result<Expiring, AuthError> {
        let token = tokio::fs::read_to_string(token_file)
            .await
            .map_err(|e| AuthError::new("web-identity", format!("cannot read token file {token_file}: {e}")))?;
        let endpoint = self
            .config
            .sts_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://sts.{}.amazonaws.com", self.config.region));
        let session_name = self.config.role_session_name.clone().unwrap_or_else(|| "graphrt-gateway".into());
        let request =
            WebIdentityRequest { endpoint: &endpoint, role_arn, session_name: &session_name, token: token.trim() };
        let response = self
            .http
            .post(format!("{}/", request.endpoint.trim_end_matches('/')))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(sts::web_identity_form(&request))
            .send()
            .await
            .map_err(|e| fetch_error("web-identity", &e))?;
        let (status, body) = read_body("web-identity", response).await?;
        sts::parse_web_identity_response(status, &body)
    }

    async fn fetch_imds(&self) -> Result<Expiring, AuthError> {
        let base = self.config.imds_endpoint.as_deref().unwrap_or(DEFAULT_IMDS_ENDPOINT).trim_end_matches('/');
        let token_response = self
            .http
            .put(format!("{base}/latest/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", IMDS_TOKEN_TTL_SECS)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|e| fetch_error("imds", &e))?;
        let (status, token) = read_body("imds", token_response).await?;
        if status != 200 {
            return Err(AuthError::new("imds", format!("IMDSv2 token request returned HTTP {status}")));
        }
        let get = |path: String| self.http.get(format!("{base}{path}")).header("x-aws-ec2-metadata-token", &token);
        let roles = get("/latest/meta-data/iam/security-credentials/".into())
            .send()
            .await
            .map_err(|e| fetch_error("imds", &e))?;
        let (status, roles) = read_body("imds", roles).await?;
        let role = roles.lines().map(str::trim).find(|l| !l.is_empty());
        let Some(role) = role.filter(|_| status == 200) else {
            return Err(AuthError::new("imds", format!("no instance role attached (HTTP {status})")));
        };
        let creds = get(format!("/latest/meta-data/iam/security-credentials/{role}"))
            .send()
            .await
            .map_err(|e| fetch_error("imds", &e))?;
        let (status, body) = read_body("imds", creds).await?;
        if status != 200 {
            return Err(AuthError::new("imds", format!("role credentials returned HTTP {status}")));
        }
        parse_json_credentials("imds", &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_lookup_builds_static_credentials() {
        let config = ChainConfig::from_lookup(|name| match name {
            "AWS_ACCESS_KEY_ID" => Some("AKID".into()),
            "AWS_SECRET_ACCESS_KEY" => Some("SECRET".into()),
            "AWS_REGION" => Some("eu-west-1".into()),
            "AWS_EC2_METADATA_DISABLED" => Some("TRUE".into()),
            "AWS_CONTAINER_AUTHORIZATION_TOKEN" => Some("container-token".into()),
            _ => None,
        });
        assert_eq!(config.static_credentials.as_ref().map(|c| c.access_key_id.as_str()), Some("AKID"));
        assert_eq!(config.region, "eu-west-1");
        assert!(config.imds_disabled);
        let debug = format!("{config:?}");
        assert!(!debug.contains("SECRET") && !debug.contains("container-token"), "{debug}");
    }

    #[test]
    fn container_full_uri_allow_list() {
        assert!(is_allowed_container_uri("http://169.254.170.23/v1/credentials"));
        assert!(is_allowed_container_uri("http://127.0.0.1:8080/creds"));
        assert!(is_allowed_container_uri("http://[fd00:ec2::23]/v1/credentials"));
        assert!(is_allowed_container_uri("https://creds.internal/x"));
        assert!(!is_allowed_container_uri("http://evil.example/creds"));
        assert!(!is_allowed_container_uri("http://169.254.169.254/latest"));
        assert!(!is_allowed_container_uri("file:///etc/passwd"));
    }

    #[test]
    fn json_credentials_require_success_code() {
        let ok = r#"{"Code":"Success","AccessKeyId":"A","SecretAccessKey":"S","Token":"T","Expiration":"2026-01-01T00:00:00Z"}"#;
        let (creds, exp) = parse_json_credentials("imds", ok).unwrap();
        assert_eq!(creds.session_token.as_deref(), Some("T"));
        assert!(exp.is_some());
        assert!(parse_json_credentials("imds", r#"{"Code":"Failed"}"#).is_err());
        assert!(parse_json_credentials("imds", "nope").is_err());
    }
}
