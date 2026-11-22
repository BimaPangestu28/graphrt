//! Entra ID access tokens without an SDK (feature `auth-azure`).
//!
//! Credentials ([`AzureCredential`]):
//! - managed identity through `IDENTITY_ENDPOINT`/`IDENTITY_HEADER` (Container
//!   Apps, App Service, Functions) or the VM/AKS node IMDS endpoint;
//! - client credentials with a client secret, or with a PS256 certificate
//!   assertion (`x5t#S256`);
//! - workload identity federation: the projected token file is re-read on every
//!   exchange, so kubelet rotation is picked up.
//!
//! [`TokenProvider`] caches one token per scope until `exp - 5 min`, refreshes
//! with a single in-flight fetch, and drops a token the API rejected with 401
//! so the caller retries exactly once. Tokens and secrets are [`SecretString`]s:
//! they never appear in `Debug`, `Display` or error messages.

mod certificate;
mod credential;
mod token_provider;

pub use certificate::CertificateKey;
pub use credential::{AuthEndpoints, AzureCredential, EnvSource};
pub use token_provider::{Clock, TokenProvider};

use crate::api::{ProviderError, SecretString};

/// OAuth scope of Azure Resource Manager.
pub const ARM_SCOPE: &str = "https://management.azure.com/.default";
/// OAuth scope of the Key Vault data plane.
pub const KEY_VAULT_SCOPE: &str = "https://vault.azure.net/.default";

/// A bearer token and its expiry on the local clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessToken {
    pub token: SecretString,
    /// Expiry in Unix seconds, local clock.
    pub expires_at: u64,
}

/// Why a token could not be obtained.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// Missing or invalid credential configuration (env vars, key file).
    #[error("azure credential configuration: {0}")]
    Config(String),
    /// The token endpoint could not be reached or kept failing transiently.
    #[error("azure token endpoint unreachable: {0}")]
    Unavailable(String),
    /// The token endpoint refused the credential (`error` code from Entra).
    #[error("azure token request rejected (HTTP {status}, {code}): {description}")]
    Rejected { status: u16, code: String, description: String },
    /// The token endpoint answered with something that is not a token.
    #[error("azure token response invalid: {0}")]
    InvalidResponse(String),
}

impl From<AuthError> for ProviderError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unavailable(message) => ProviderError::Transient(message),
            other => ProviderError::Auth(other.to_string()),
        }
    }
}
