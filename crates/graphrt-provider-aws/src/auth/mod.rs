//! `auth-aws`: SigV4, the AWS credential chain and presigned EKS tokens.
//!
//! Hand-rolled on `sha2` + `reqwest` (no AWS SDK, plan §6). The chain resolves,
//! in order:
//!
//! 1. environment (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`);
//! 2. the container credential endpoint (ECS task role via
//!    `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, EKS Pod Identity via
//!    `AWS_CONTAINER_CREDENTIALS_FULL_URI` + authorization token);
//! 3. web identity (`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`, EKS IRSA) through
//!    STS `AssumeRoleWithWebIdentity`, parsed with a minimal XML reader;
//! 4. IMDSv2 on EC2 (session token, then the instance role).
//!
//! Temporary credentials are cached until `expiration − 5 min`, refreshed by one
//! caller at a time, invalidated once on a 401/403, and never logged.

mod clock;
mod credentials;
mod presign;
pub mod sigv4;
mod sts;

pub use self::clock::Clock;
pub(crate) use self::clock::parse_http_date;
pub use self::credentials::{ChainConfig, CredentialChain, CredentialSource};
pub use self::presign::eks_token;
pub use self::sigv4::AwsCredentials;
pub use self::sts::{parse_iso8601, xml_text};

use std::fmt;

/// Why credentials could not be obtained. Messages never contain secret material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthError {
    pub source: &'static str,
    pub message: String,
}

impl AuthError {
    pub(crate) fn new(source: &'static str, message: impl Into<String>) -> Self {
        Self { source, message: message.into() }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "aws credentials ({}): {}", self.source, self.message)
    }
}

impl std::error::Error for AuthError {}
