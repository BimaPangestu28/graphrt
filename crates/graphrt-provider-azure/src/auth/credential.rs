//! The credential kinds and the one token request each of them makes.

use crate::http::json_at;
use std::path::PathBuf;
use std::time::Duration;

use super::{AccessToken, AuthError, CertificateKey};
use graphrt_provider_api::http::{JitterRng, RetryPolicy, SecretString};

use crate::http::{form_encode, is_retryable_status, percent_encode, read_body, retry_after};

const CLIENT_ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// api-version of the App Service / Container Apps identity endpoint.
const IDENTITY_ENDPOINT_API_VERSION: &str = "2019-08-01";
/// api-version of the VM instance metadata identity endpoint.
const IMDS_API_VERSION: &str = "2018-02-01";

/// Token endpoints, from the gateway operator's configuration (never from a
/// deployment spec, so a tenant cannot point a credential at another host).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthEndpoints {
    /// Entra authority, e.g. `https://login.microsoftonline.com` (sovereign
    /// clouds: `https://login.microsoftonline.us`, `https://login.chinacloudapi.cn`).
    pub authority_host: String,
    /// VM IMDS base URL.
    pub imds_endpoint: String,
}

impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            authority_host: "https://login.microsoftonline.com".into(),
            imds_endpoint: "http://169.254.169.254".into(),
        }
    }
}

/// Reads one environment variable; injectable for tests.
pub type EnvSource<'a> = &'a dyn Fn(&str) -> Option<String>;

/// One way of obtaining Entra tokens.
#[derive(Clone, Debug)]
pub enum AzureCredential {
    /// `IDENTITY_ENDPOINT` + `IDENTITY_HEADER` (Container Apps, App Service).
    AppServiceIdentity { endpoint: String, header: SecretString, client_id: Option<String> },
    /// VM / VMSS / AKS node instance metadata service.
    Imds { endpoint: String, client_id: Option<String> },
    /// Client credentials with a client secret.
    ClientSecret { authority_host: String, tenant_id: String, client_id: String, secret: SecretString },
    /// Client credentials with a PS256 certificate assertion.
    ClientCertificate { authority_host: String, tenant_id: String, client_id: String, key: CertificateKey },
    /// Workload identity federation: the federated token file is the assertion.
    WorkloadIdentity { authority_host: String, tenant_id: String, client_id: String, token_file: PathBuf },
}

impl AzureCredential {
    /// The credential the environment describes, in the order the Azure SDKs use:
    /// workload identity, client secret, client certificate, then managed
    /// identity (`IDENTITY_ENDPOINT`, else IMDS). `AZURE_AUTHORITY_HOST` is
    /// honoured only for workload identity (the AKS webhook sets it); other
    /// hosts come from `endpoints`.
    ///
    /// @throws `AuthError::Config` when the certificate file cannot be read or parsed.
    pub fn from_env(env: EnvSource<'_>, endpoints: &AuthEndpoints) -> Result<Self, AuthError> {
        let tenant_id = env("AZURE_TENANT_ID");
        let client_id = env("AZURE_CLIENT_ID");
        let authority_host = endpoints.authority_host.clone();
        if let (Some(token_file), Some(tenant_id), Some(client_id)) =
            (env("AZURE_FEDERATED_TOKEN_FILE"), tenant_id.clone(), client_id.clone())
        {
            let authority_host = env("AZURE_AUTHORITY_HOST").unwrap_or(authority_host);
            return Ok(Self::WorkloadIdentity { authority_host, tenant_id, client_id, token_file: token_file.into() });
        }
        if let (Some(secret), Some(tenant_id), Some(client_id)) =
            (env("AZURE_CLIENT_SECRET"), tenant_id.clone(), client_id.clone())
        {
            return Ok(Self::ClientSecret { authority_host, tenant_id, client_id, secret: SecretString::new(secret) });
        }
        if let (Some(path), Some(tenant_id), Some(client_id)) =
            (env("AZURE_CLIENT_CERTIFICATE_PATH"), tenant_id, client_id.clone())
        {
            let pem = std::fs::read_to_string(&path).map_err(|error| {
                AuthError::Config(format!("cannot read AZURE_CLIENT_CERTIFICATE_PATH {path}: {error}"))
            })?;
            let key = CertificateKey::from_pem(&pem)?;
            return Ok(Self::ClientCertificate { authority_host, tenant_id, client_id, key });
        }
        if let (Some(endpoint), Some(header)) = (env("IDENTITY_ENDPOINT"), env("IDENTITY_HEADER")) {
            return Ok(Self::AppServiceIdentity { endpoint, header: SecretString::new(header), client_id });
        }
        Ok(Self::Imds { endpoint: endpoints.imds_endpoint.clone(), client_id })
    }

    /// Short name for diagnostics (never includes a secret).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AppServiceIdentity { .. } => "managed-identity",
            Self::Imds { .. } => "imds",
            Self::ClientSecret { .. } => "client-secret",
            Self::ClientCertificate { .. } => "client-certificate",
            Self::WorkloadIdentity { .. } => "workload-identity",
        }
    }

    /// Fetch a token for `scope` (`<resource>/.default`), retrying transient
    /// failures (429, 5xx, IMDS 404/410 while it starts) with jittered backoff.
    ///
    /// @throws `AuthError::Rejected` when Entra refuses the credential,
    ///   `AuthError::Unavailable` when the endpoint keeps failing.
    pub(crate) async fn fetch(
        &self,
        client: &reqwest::Client,
        scope: &str,
        now_unix: u64,
    ) -> Result<AccessToken, AuthError> {
        let policy = RetryPolicy { max_attempts: 3, ..RetryPolicy::default() };
        let jitter = JitterRng::from_entropy();
        let mut attempt = 1;
        loop {
            match self.fetch_once(client, scope, now_unix).await {
                Err(Retryable { hint, .. }) if attempt < policy.max_attempts => {
                    tokio::time::sleep(policy.delay(attempt - 1, hint, &jitter)).await;
                    attempt += 1;
                }
                Err(Retryable { error, .. }) => return Err(error),
                Ok(Ok(token)) => return Ok(token),
                Ok(Err(error)) => return Err(error),
            }
        }
    }

    async fn fetch_once(
        &self,
        client: &reqwest::Client,
        scope: &str,
        now_unix: u64,
    ) -> Result<Result<AccessToken, AuthError>, Retryable> {
        let request = match self.build_request(client, scope, now_unix).await {
            Ok(request) => request,
            Err(error) => return Ok(Err(error)),
        };
        let response = request.send().await.map_err(|error| Retryable {
            hint: None,
            error: AuthError::Unavailable(format!("{} token request failed: {}", self.kind(), error.without_url())),
        })?;
        let status = response.status().as_u16();
        let hint = retry_after(response.headers());
        let body =
            read_body(response).await.map_err(|error| Retryable { hint, error: AuthError::Unavailable(error) })?;
        let imds_booting = matches!(self, Self::Imds { .. }) && matches!(status, 404 | 410);
        if is_retryable_status(status) || imds_booting {
            return Err(Retryable {
                hint,
                error: AuthError::Unavailable(format!("{} token endpoint answered HTTP {status}", self.kind())),
            });
        }
        if !(200..300).contains(&status) {
            return Ok(Err(rejection(status, &body)));
        }
        Ok(parse_token_response(&body, now_unix))
    }

    async fn build_request(
        &self,
        client: &reqwest::Client,
        scope: &str,
        now_unix: u64,
    ) -> Result<reqwest::RequestBuilder, AuthError> {
        let resource = scope.strip_suffix("/.default").unwrap_or(scope);
        let request = match self {
            Self::AppServiceIdentity { endpoint, header, client_id } => {
                let mut url = format!(
                    "{endpoint}?api-version={IDENTITY_ENDPOINT_API_VERSION}&resource={}",
                    percent_encode(resource)
                );
                if let Some(client_id) = client_id {
                    url.push_str(&format!("&client_id={}", percent_encode(client_id)));
                }
                client.get(url).header("X-IDENTITY-HEADER", header.expose())
            }
            Self::Imds { endpoint, client_id } => {
                let mut url = format!(
                    "{}/metadata/identity/oauth2/token?api-version={IMDS_API_VERSION}&resource={}",
                    endpoint.trim_end_matches('/'),
                    percent_encode(resource)
                );
                if let Some(client_id) = client_id {
                    url.push_str(&format!("&client_id={}", percent_encode(client_id)));
                }
                client.get(url).header("Metadata", "true")
            }
            Self::ClientSecret { authority_host, tenant_id, client_id, secret } => {
                let body = form_encode(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", client_id),
                    ("client_secret", secret.expose()),
                    ("scope", scope),
                ]);
                form_post(client, &token_url(authority_host, tenant_id), body)
            }
            Self::ClientCertificate { authority_host, tenant_id, client_id, key } => {
                let url = token_url(authority_host, tenant_id);
                let assertion = key.assertion(client_id, &url, now_unix)?;
                form_post(client, &url, assertion_form(client_id, &assertion, scope))
            }
            Self::WorkloadIdentity { authority_host, tenant_id, client_id, token_file } => {
                // Re-read every time: the kubelet rotates the projected token.
                let assertion = tokio::fs::read_to_string(token_file).await.map_err(|error| {
                    AuthError::Config(format!(
                        "cannot read AZURE_FEDERATED_TOKEN_FILE {}: {error}",
                        token_file.display()
                    ))
                })?;
                let url = token_url(authority_host, tenant_id);
                form_post(client, &url, assertion_form(client_id, assertion.trim(), scope))
            }
        };
        Ok(request.timeout(Duration::from_secs(10)))
    }
}

/// A failure worth retrying, with the server's `Retry-After` hint.
struct Retryable {
    hint: Option<Duration>,
    error: AuthError,
}

fn token_url(authority_host: &str, tenant_id: &str) -> String {
    format!("{}/{}/oauth2/v2.0/token", authority_host.trim_end_matches('/'), percent_encode(tenant_id))
}

fn assertion_form(client_id: &str, assertion: &str, scope: &str) -> String {
    form_encode(&[
        ("grant_type", "client_credentials"),
        ("client_id", client_id),
        ("client_assertion_type", CLIENT_ASSERTION_TYPE),
        ("client_assertion", assertion),
        ("scope", scope),
    ])
}

fn form_post(client: &reqwest::Client, url: &str, body: String) -> reqwest::RequestBuilder {
    client.post(url).header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(body)
}

/// Entra's `{error, error_description}`, truncated; never the request.
fn rejection(status: u16, body: &[u8]) -> AuthError {
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let code = json_at(&parsed, "/error").as_str().unwrap_or("unknown").chars().take(64).collect();
    let description = json_at(&parsed, "/error_description")
        .as_str()
        .or_else(|| json_at(&parsed, "/message").as_str())
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    AuthError::Rejected { status, code, description }
}

/// A token response of any of the endpoints. `expires_in` (relative) is
/// preferred over `expires_on` (absolute) so local clock skew cannot make a
/// token look valid longer than it is.
fn parse_token_response(body: &[u8], now_unix: u64) -> Result<AccessToken, AuthError> {
    let parsed: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| AuthError::InvalidResponse("token response is not JSON".into()))?;
    let token = json_at(&parsed, "/access_token")
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AuthError::InvalidResponse("token response has no access_token".into()))?;
    let expires_at =
        match (number_field(json_at(&parsed, "/expires_in")), number_field(json_at(&parsed, "/expires_on"))) {
            (Some(expires_in), _) => now_unix + expires_in,
            (None, Some(expires_on)) => expires_on,
            (None, None) => return Err(AuthError::InvalidResponse("token response has no expiry".into())),
        };
    Ok(AccessToken { token: SecretString::new(token), expires_at })
}

/// A JSON number or a numeric string (managed identity endpoints send strings).
fn number_field(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_string())
    }

    #[test]
    fn env_chain_prefers_workload_identity_then_secret_then_managed_identity() {
        let endpoints = AuthEndpoints::default();
        let workload = env_of(&[
            ("AZURE_TENANT_ID", "t"),
            ("AZURE_CLIENT_ID", "c"),
            ("AZURE_FEDERATED_TOKEN_FILE", "/var/run/token"),
            ("AZURE_CLIENT_SECRET", "s"),
            ("AZURE_AUTHORITY_HOST", "https://login.microsoftonline.us/"),
        ]);
        let credential = AzureCredential::from_env(&workload, &endpoints).unwrap();
        assert_eq!(credential.kind(), "workload-identity");
        assert!(
            matches!(credential, AzureCredential::WorkloadIdentity { ref authority_host, .. } if authority_host.ends_with(".us/"))
        );

        let secret = env_of(&[("AZURE_TENANT_ID", "t"), ("AZURE_CLIENT_ID", "c"), ("AZURE_CLIENT_SECRET", "s")]);
        assert_eq!(AzureCredential::from_env(&secret, &endpoints).unwrap().kind(), "client-secret");

        let managed = env_of(&[("IDENTITY_ENDPOINT", "http://localhost:42356/msi/token"), ("IDENTITY_HEADER", "h")]);
        assert_eq!(AzureCredential::from_env(&managed, &endpoints).unwrap().kind(), "managed-identity");

        assert_eq!(AzureCredential::from_env(&env_of(&[]), &endpoints).unwrap().kind(), "imds");
    }

    #[test]
    fn debug_never_shows_secrets() {
        let credential = AzureCredential::ClientSecret {
            authority_host: "https://login.microsoftonline.com".into(),
            tenant_id: "t".into(),
            client_id: "c".into(),
            secret: SecretString::new("super-secret-value"),
        };
        assert!(!format!("{credential:?}").contains("super-secret-value"));
    }

    #[test]
    fn expiry_prefers_relative_expires_in_and_accepts_strings() {
        let relative = parse_token_response(br#"{"access_token":"a","expires_in":"3599","expires_on":"1"}"#, 1000);
        assert_eq!(relative.map(|token| token.expires_at), Ok(4599));
        let absolute = parse_token_response(br#"{"access_token":"a","expires_on":"1586984735"}"#, 1000);
        assert_eq!(absolute.map(|token| token.expires_at), Ok(1_586_984_735));
        assert!(parse_token_response(br#"{"access_token":"a"}"#, 0).is_err());
        assert!(parse_token_response(br#"{"expires_in":1}"#, 0).is_err());
    }

    #[test]
    fn rejection_keeps_code_and_first_description_line_only() {
        let error = rejection(
            401,
            br#"{"error":"invalid_client","error_description":"AADSTS7000215: bad secret.\r\nTrace ID: x"}"#,
        );
        assert_eq!(
            error,
            AuthError::Rejected {
                status: 401,
                code: "invalid_client".into(),
                description: "AADSTS7000215: bad secret.".into()
            }
        );
    }
}
