//! Direct deploys to AWS Lambda: an agent served straight from a Lambda
//! Function URL (`https://<id>.lambda-url.<region>.on.aws`), with no graphrt
//! gateway and nothing running while idle (`graphrt deploy --target lambda
//! --direct`, see `docs/deploy-direct-aws.md`).
//!
//! One Lambda function per deployment, named `graphrt-<deployment>`:
//!
//! - **Image.** Lambda runs images from ECR of its own account only. A deploy
//!   copies the graphrt image (GHCR by default) into the ECR repository of the
//!   deployment and adds one layer: the Lambda Web Adapter extension and the
//!   agent file under `/workflows`. No Docker is needed and only new layers are
//!   uploaded. Every version is one image tag.
//! - **Versions.** Each deploy publishes an immutable Lambda version whose
//!   description carries the version label. The alias `live` points at the
//!   current one and owns the public Function URL (`AuthType NONE`,
//!   `RESPONSE_STREAM` for SSE): graphrt's API key protects the agent.
//!   A new version is health checked (`GET /readyz`) by invoking it directly
//!   before `live` moves, so a broken version never serves.
//! - **State.** The API key and the `--secret` values are environment
//!   variables of the function (encrypted at rest by Lambda), kept from one
//!   deploy to the next. Lambda keeps no disk between requests, so the features
//!   that need `graphrt serve --store` are refused.
//! - **Pause** sets the reserved concurrency to 0 (every request gets 429),
//!   **remove** deletes the function, its ECR repository, its role (when graphrt
//!   made it) and its log group.

mod account;
mod layer;
mod lifecycle;
pub mod registry;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::api::{ProviderError, ProviderErrorKind};
use crate::auth::CredentialChain;
use crate::rest::{ApiLimits, AwsRestClient, ServiceEndpoint};

pub use crate::api::direct::{AgentFile, Step, StepEvent, StepStatus};
pub use lifecycle::{DeployFailure, DeploymentState, DeploymentStatus, Progress, ServingVersion, VersionSummary};

/// Environment variable holding the deployment's API key (`graphrt serve` reads it).
pub const API_KEY_ENV: &str = "GRAPHRT_API_KEY";
/// The alias that serves and owns the Function URL.
pub const LIVE_ALIAS: &str = "live";
/// Port `graphrt serve` listens on behind the adapter.
pub const AGENT_PORT: u16 = 8080;
/// Directory of the agent inside the image.
pub const WORKFLOW_DIR: &str = "/workflows";
/// Lambda Web Adapter image whose binary becomes the function's extension.
pub const DEFAULT_ADAPTER_IMAGE: &str = "public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0";
/// Versions kept for rollback; older ones and their images are deleted after a deploy.
pub const DEFAULT_KEEP_VERSIONS: usize = 5;
/// Lambda's cap on one request (the function timeout).
pub const FUNCTION_TIMEOUT_SECS: u64 = 900;

/// One `graphrt deploy --target lambda --direct` call.
#[derive(Clone)]
pub struct DeployRequest {
    /// Deployment name (`GRAPHRT_DEPLOYMENT`).
    pub deployment: String,
    /// Version label: `[A-Za-z0-9._-]`, 1-64 characters (it is also the image tag).
    pub version: String,
    /// Image running `graphrt serve` (any public registry, or ECR of this account).
    pub image: String,
    /// Image of the Lambda Web Adapter ([`DEFAULT_ADAPTER_IMAGE`]).
    pub adapter_image: String,
    /// Secrets to set: name → value (never logged or returned). Earlier ones are kept.
    pub secrets: BTreeMap<String, String>,
    /// A fresh API key, used when the function has none yet or `rotate_api_key` is set.
    pub api_key_candidate: String,
    pub rotate_api_key: bool,
    pub agent: AgentFile,
    /// Name the agent is served under (`POST /v1/workflows/{workflow}/runs`).
    pub workflow: String,
    /// Flags after `graphrt serve /workflows --addr ...` (chat page, CORS...).
    pub serve_flags: Vec<String>,
    /// What the version serves besides the API, as the result reports it.
    pub serve_summary: serde_json::Value,
    /// An existing execution role to use instead of the one graphrt creates.
    pub role_arn: Option<String>,
    /// Wall clock of the deploy (Unix ms).
    pub now_ms: u64,
}

impl std::fmt::Debug for DeployRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployRequest")
            .field("deployment", &self.deployment)
            .field("version", &self.version)
            .field("image", &self.image)
            .field("secret_names", &self.secrets.keys().collect::<Vec<_>>())
            .field("agent", &self.agent)
            .field("serve_flags", &self.serve_flags)
            .finish_non_exhaustive()
    }
}

/// What a successful deploy produced.
#[derive(Clone, Serialize)]
pub struct DeployOutcome {
    /// Function name.
    pub app: String,
    pub region: String,
    pub url: String,
    pub version: String,
    pub workflow: String,
    /// Only when this deploy created or rotated the key (shown once).
    pub api_key: Option<String>,
    pub serve: serde_json::Value,
    /// Always `null`: Lambda keeps no store.
    pub store: Option<()>,
    /// The deployment was paused and this deploy put it back online.
    pub resumed: bool,
}

impl std::fmt::Debug for DeployOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployOutcome")
            .field("app", &self.app)
            .field("version", &self.version)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// Stable error codes (the `code` of a JSON `error` line).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A flag or the agent file is invalid.
    InvalidInput,
    /// No AWS credentials were found.
    MissingCredentials,
    /// The credentials are wrong or expired.
    Unauthorized,
    /// The credentials work but may not do something the deploy needs.
    PermissionDenied,
    /// The region does not exist, is not enabled for the account, or has no Lambda.
    RegionUnavailable,
    /// The account is new and not activated yet, or must opt in first.
    AccountNotReady,
    /// An AWS quota is used up (storage, concurrency, number of functions).
    QuotaExceeded,
    /// The deployment or version does not exist.
    NotFound,
    /// AWS keeps throttling the calls.
    RateLimited,
    /// The graphrt or adapter image could not be read.
    ImageUnavailable,
    /// The new version never answered `/readyz`; the previous one keeps serving.
    HealthCheckFailed,
    /// AWS is unreachable or answered 5xx.
    AwsUnavailable,
    /// AWS refused a call for another reason.
    AwsApi,
    /// A local failure (no random source, no HTTP client).
    Internal,
}

impl ErrorCode {
    /// The snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::MissingCredentials => "missing_credentials",
            Self::Unauthorized => "unauthorized",
            Self::PermissionDenied => "permission_denied",
            Self::RegionUnavailable => "region_unavailable",
            Self::AccountNotReady => "account_not_ready",
            Self::QuotaExceeded => "quota_exceeded",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::ImageUnavailable => "image_unavailable",
            Self::HealthCheckFailed => "health_check_failed",
            Self::AwsUnavailable => "aws_unavailable",
            Self::AwsApi => "aws_api",
            Self::Internal => "internal",
        }
    }
}

/// A failed operation. Messages never contain credentials or a secret value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectError {
    pub code: ErrorCode,
    pub message: String,
}

impl DirectError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }

    /// An AWS failure while trying to `action` ("create the function"), with the
    /// fix a person without AWS experience can follow.
    pub fn from_aws(error: ProviderError, action: &str) -> Self {
        let aws_code = error.message.split(" (HTTP").next().unwrap_or_default().to_owned();
        let detail = error.message.clone();
        let lower = detail.to_ascii_lowercase();
        let (code, advice) = if detail.starts_with("aws credentials (") {
            (
                ErrorCode::MissingCredentials,
                "No AWS credentials were found. Run `aws configure` (or `aws configure --profile <name>` and pass \
                 --profile <name>), or set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY."
                    .to_owned(),
            )
        } else if matches!(
            aws_code.as_str(),
            "InvalidClientTokenId"
                | "SignatureDoesNotMatch"
                | "ExpiredToken"
                | "ExpiredTokenException"
                | "UnrecognizedClientException"
                | "InvalidSignatureException"
        ) || lower.contains("security token included in the request is invalid")
        {
            (
                ErrorCode::Unauthorized,
                "The AWS access key is wrong, deleted or expired. Create a new access key in the AWS console \
                 (IAM, Users, Security credentials) and set it again, or pick another --profile. If the key is \
                 right, the region may not be enabled for this account (AWS console, Account, AWS Regions)."
                    .to_owned(),
            )
        } else if aws_code.contains("AccessDenied")
            || aws_code == "UnauthorizedOperation"
            || lower.contains("not authorized to perform")
        {
            (
                ErrorCode::PermissionDenied,
                "The AWS user may not do this. Ask your AWS administrator to attach the graphrt deploy policy \
                 (docs/deploy-direct-aws.md, section IAM policy) to the user or role, then deploy again."
                    .to_owned(),
            )
        } else if matches!(aws_code.as_str(), "OptInRequired" | "SubscriptionRequiredException" | "AccountBlocked")
            || lower.contains("not subscribed")
            || lower.contains("pending verification")
        {
            (
                ErrorCode::AccountNotReady,
                "The AWS account is not ready for Lambda yet. A new account can take up to 24 hours to be \
                 activated and needs a verified payment method; check the AWS console (Billing) and try again later."
                    .to_owned(),
            )
        } else if aws_code.contains("LimitExceeded")
            || aws_code == "CodeStorageExceededException"
            || aws_code == "ServiceQuotaExceededException"
            || lower.contains("quota")
        {
            (
                ErrorCode::QuotaExceeded,
                "An AWS quota is used up. Remove deployments you no longer need (graphrt deploy remove), or ask \
                 AWS for more in the console (Service Quotas, AWS Lambda or Amazon ECR)."
                    .to_owned(),
            )
        } else {
            let code = match error.kind {
                ProviderErrorKind::Auth => ErrorCode::PermissionDenied,
                ProviderErrorKind::NotFound => ErrorCode::NotFound,
                ProviderErrorKind::Capacity => ErrorCode::QuotaExceeded,
                ProviderErrorKind::RateLimited => ErrorCode::RateLimited,
                ProviderErrorKind::Transient => ErrorCode::AwsUnavailable,
                _ => ErrorCode::AwsApi,
            };
            let advice = match code {
                ErrorCode::RateLimited => "AWS is throttling the calls: wait a minute and try again.".to_owned(),
                ErrorCode::AwsUnavailable if lower.contains("dns") || lower.contains("connect") => {
                    "AWS could not be reached: check the internet connection and the region code \
                     (for example ap-southeast-1)."
                        .to_owned()
                }
                ErrorCode::AwsUnavailable => "AWS did not answer properly: try again in a minute.".to_owned(),
                _ => String::new(),
            };
            (code, advice)
        };
        let message = if advice.is_empty() {
            format!("could not {action}: {detail}")
        } else {
            format!("could not {action}: {detail}. {advice}")
        };
        Self::new(code, message)
    }
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for DirectError {}

/// Whether `region` looks like an AWS region code (`ap-southeast-1`).
pub fn is_region_code(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    parts.len() >= 3
        && parts.first().is_some_and(|first| first.len() == 2 && first.bytes().all(|b| b.is_ascii_lowercase()))
        && parts.last().is_some_and(|last| !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()))
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// The function name of a deployment: `graphrt-<slug>`, at most 64 characters.
///
/// @throws DirectError(InvalidInput) - nothing usable is left after sanitising
pub fn function_name_for(deployment: &str) -> Result<String, DirectError> {
    let mut slug = String::new();
    for character in deployment.chars().flat_map(char::to_lowercase) {
        let mapped = if character.is_ascii_lowercase() || character.is_ascii_digit() { character } else { '-' };
        if mapped == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(mapped);
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        return Err(DirectError::invalid(format!("deployment name {deployment:?} has no letters or digits")));
    }
    let mut name = format!("graphrt-{slug}");
    name.truncate(56);
    Ok(name.trim_end_matches('-').to_owned())
}

/// Endpoints, timing and limits of the deployer.
#[derive(Clone, Debug)]
pub struct DirectSettings {
    pub region: String,
    pub lambda: ServiceEndpoint,
    pub ecr: ServiceEndpoint,
    pub iam: ServiceEndpoint,
    pub sts: ServiceEndpoint,
    pub logs: ServiceEndpoint,
    pub limits: ApiLimits,
    /// Longest wait for a function update or a new role.
    pub ready_timeout: Duration,
    /// Longest wait for a new version's `/readyz`.
    pub health_timeout: Duration,
    pub poll_interval: Duration,
    pub keep_versions: usize,
}

impl DirectSettings {
    /// Public endpoints of `region`.
    pub fn new(region: &str) -> Self {
        Self {
            region: region.to_owned(),
            lambda: ServiceEndpoint::regional("lambda", region),
            ecr: ServiceEndpoint {
                base_url: format!("https://api.ecr.{region}.amazonaws.com"),
                region: region.to_owned(),
                service: "ecr".to_owned(),
            },
            iam: ServiceEndpoint {
                base_url: "https://iam.amazonaws.com".to_owned(),
                region: "us-east-1".to_owned(),
                service: "iam".to_owned(),
            },
            sts: ServiceEndpoint::regional("sts", region),
            logs: ServiceEndpoint::regional("logs", region),
            limits: ApiLimits::default(),
            ready_timeout: Duration::from_secs(300),
            health_timeout: Duration::from_secs(180),
            poll_interval: Duration::from_secs(2),
            keep_versions: DEFAULT_KEEP_VERSIONS,
        }
    }
}

/// The direct deployer of one Lambda function.
pub struct LambdaDirect {
    settings: DirectSettings,
    function: String,
    chain: Arc<CredentialChain>,
    lambda: AwsRestClient,
    ecr: AwsRestClient,
    iam: AwsRestClient,
    sts: AwsRestClient,
    logs: AwsRestClient,
}

impl std::fmt::Debug for LambdaDirect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LambdaDirect")
            .field("function", &self.function)
            .field("region", &self.settings.region)
            .finish_non_exhaustive()
    }
}

impl LambdaDirect {
    /// A deployer of `function` signing with `chain`.
    ///
    /// @throws DirectError(Internal) - an HTTP client cannot be built
    pub fn new(settings: DirectSettings, function: String, chain: Arc<CredentialChain>) -> Result<Self, DirectError> {
        let client = |endpoint: &ServiceEndpoint| {
            AwsRestClient::new(Arc::clone(&chain), endpoint.clone(), settings.limits)
                .map_err(|error| DirectError::new(ErrorCode::Internal, error.message))
        };
        Ok(Self {
            lambda: client(&settings.lambda)?,
            ecr: client(&settings.ecr)?,
            iam: client(&settings.iam)?,
            sts: client(&settings.sts)?,
            logs: client(&settings.logs)?,
            settings,
            function,
            chain,
        })
    }

    /// The function this deployer manages.
    pub fn function_name(&self) -> &str {
        &self.function
    }

    pub fn region(&self) -> &str {
        &self.settings.region
    }

    fn repository(&self) -> String {
        self.function.clone()
    }

    fn role_name(&self) -> String {
        let mut name = format!("{}-lambda", self.function);
        name.truncate(64);
        name
    }

    fn log_group(&self) -> String {
        format!("/aws/lambda/{}", self.function)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_names_are_sanitised_and_prefixed() {
        assert_eq!(function_name_for("Support Bot!").unwrap(), "graphrt-support-bot");
        assert!(function_name_for("!!").is_err());
        assert!(function_name_for(&"a".repeat(200)).unwrap().len() <= 56);
    }

    #[test]
    fn region_codes() {
        assert!(is_region_code("ap-southeast-1") && is_region_code("us-gov-west-1"));
        assert!(!is_region_code("singapore") && !is_region_code("AP-SOUTHEAST-1") && !is_region_code("ap-southeast"));
    }

    #[test]
    fn aws_errors_become_codes_with_a_fix() {
        let denied = DirectError::from_aws(
            ProviderError::auth(
                "AccessDeniedException (HTTP 403): User: x is not authorized to perform: lambda:CreateFunction",
            ),
            "create the function",
        );
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
        assert!(denied.message.contains("IAM policy") && denied.message.contains("lambda:CreateFunction"));
        let bad_key = DirectError::from_aws(
            ProviderError::auth(
                "UnrecognizedClientException (HTTP 403): The security token included in the request is invalid.",
            ),
            "check the AWS credentials",
        );
        assert_eq!(bad_key.code, ErrorCode::Unauthorized);
        let quota = DirectError::from_aws(
            ProviderError::fatal("CodeStorageExceededException (HTTP 400): Code storage limit exceeded."),
            "create the function",
        );
        assert_eq!(quota.code, ErrorCode::QuotaExceeded);
        let new_account = DirectError::from_aws(
            ProviderError::fatal(
                "SubscriptionRequiredException (HTTP 400): The AWS Access Key Id needs a subscription",
            ),
            "create the function",
        );
        assert_eq!(new_account.code, ErrorCode::AccountNotReady);
        assert_eq!(DirectError::from_aws(ProviderError::transient("x"), "y").code, ErrorCode::AwsUnavailable);
    }
}
