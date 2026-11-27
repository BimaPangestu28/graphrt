//! Direct deploys to Google Cloud Run: one agent served straight by Cloud Run
//! at its `https://<service>-<number>.<region>.run.app` address, with no
//! graphrt gateway (`graphrt deploy --target cloud-run --direct`, see
//! `docs/deploy-cloud-run.md`).
//!
//! What a deployment is made of, all in the person's own project and region:
//!
//! | resource | name | holds |
//! | --- | --- | --- |
//! | Cloud Run service | the deployment name (`--app`) | one revision per version, a traffic tag per kept version |
//! | Cloud Storage bucket | `<project>-<service>` | `agents/<sha>/` (the agent of each version), `store/` (`graphrt serve --store`) |
//! | service account | `graphrt-<service>` | what the service runs as: reads its secrets and its bucket, nothing else |
//! | Secret Manager secrets | `<service>-<name>` | the API key and every `--secret` |
//! | Artifact Registry remote repository | `graphrt-ghcr` (one per region) | a pull through cache of `ghcr.io`, which Cloud Run cannot pull from |
//!
//! No image is built: every revision runs the published graphrt image (through
//! the remote repository), and the agent file is uploaded to the bucket, which
//! the revision mounts read only at `/workflows` (`only-dir=agents/<sha>`), so a
//! revision keeps its own agent and a rollback serves the agent it was deployed with.
//!
//! A deploy creates the new revision without traffic, waits until Cloud Run
//! reports it ready (its startup probe is `GET /readyz`, Cloud Run keeps
//! `/healthz` for itself), then moves all traffic to it and checks
//! `GET <url>/readyz` from outside. A revision that does not get ready gets no
//! traffic: the previous version keeps serving. Rollback moves the traffic back
//! to a tagged revision. Pause takes the service off the internet (internal
//! ingress, invoker IAM on again, no instances), resume undoes it, remove
//! deletes all of the above but the shared remote repository.
//!
//! The service is public (`invokerIamDisabled`, so no `allUsers` IAM binding is
//! needed, which organizations with domain restricted sharing forbid); graphrt
//! itself asks for the deployment's API key. With a store the service runs at
//! most one instance: Cloud Storage FUSE has no locks across instances.

#[cfg(feature = "fake")]
pub mod fake;
mod google;
mod lifecycle;
mod naming;
mod resources;
mod service;

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_api::direct::{AgentFile, ServeOptions, ServeSummary, ServiceMode, StepEvent};
use serde::Serialize;

use crate::auth::GcpAuth;

pub use google::Endpoints;
pub use lifecycle::{DeployFailure, Progress, RemoveOutcome};
pub use naming::{bucket_name_for, service_account_id_for, service_name_for};

/// Name of the environment variable `graphrt serve` reads the API key from.
pub const API_KEY_ENV: &str = "GRAPHRT_API_KEY";
/// Versions kept for rollback (traffic tags); older ones lose their tag.
pub const DEFAULT_KEEP_VERSIONS: usize = 5;
/// Region used when none is given: Cloud Run's first tier price, close to the US.
pub const DEFAULT_REGION: &str = "us-central1";
/// Artifact Registry remote repository that proxies `ghcr.io` (one per region).
pub const GHCR_REMOTE_REPOSITORY: &str = "graphrt-ghcr";

/// Stable error codes of the Cloud Run direct deployer (the `code` of a JSON `error` line).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A flag or the agent file is invalid.
    InvalidInput,
    /// Google rejected the credentials (expired or revoked sign in).
    Unauthorized,
    /// The credentials lack an IAM permission in the project.
    PermissionDenied,
    /// A Google API the deploy needs is off and could not be turned on.
    ApiDisabled,
    /// The project has no active billing account.
    BillingDisabled,
    /// An organization policy forbids something the deploy needs.
    OrgPolicy,
    /// A name the deployment needs belongs to someone else (a bucket, a service).
    NameTaken,
    /// The deployment, version or project does not exist.
    NotFound,
    /// A quota is used up, or Google keeps rate limiting the calls.
    RateLimited,
    /// A revision or an operation did not finish in time.
    StartTimeout,
    /// The new revision never got ready; the previous version (if any) keeps serving.
    HealthCheckFailed,
    /// Google Cloud is unreachable or answered 5xx.
    GcpUnavailable,
    /// Google Cloud refused a call for another reason.
    GcpApi,
    /// A local failure (no random source, no async runtime).
    Internal,
}

impl ErrorCode {
    /// The snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Unauthorized => "unauthorized",
            Self::PermissionDenied => "permission_denied",
            Self::ApiDisabled => "api_disabled",
            Self::BillingDisabled => "billing_disabled",
            Self::OrgPolicy => "org_policy",
            Self::NameTaken => "name_taken",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::StartTimeout => "start_timeout",
            Self::HealthCheckFailed => "health_check_failed",
            Self::GcpUnavailable => "gcp_unavailable",
            Self::GcpApi => "gcp_api",
            Self::Internal => "internal",
        }
    }
}

/// A failed operation. Messages never contain a token or a secret value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectError {
    pub code: ErrorCode,
    pub message: String,
}

impl DirectError {
    /// An error with `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// Invalid input.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for DirectError {}

/// One `graphrt deploy --target cloud-run --direct` call.
#[derive(Clone)]
pub struct DeployRequest {
    /// Deployment name (`GRAPHRT_DEPLOYMENT`, labels).
    pub deployment: String,
    /// Version label of this deploy.
    pub version: String,
    /// Always-on (one instance kept warm) or scale-to-zero.
    pub mode: ServiceMode,
    /// Image running `graphrt serve`; `None` = the published graphrt image of this version.
    pub image: Option<String>,
    /// Secrets to store: name -> value (never logged or returned).
    pub secrets: BTreeMap<String, String>,
    /// A freshly generated API key, stored when the deployment has none yet or
    /// `rotate_api_key` is set (and then returned once).
    pub api_key_candidate: String,
    /// Replace an existing API key.
    pub rotate_api_key: bool,
    /// The agent.
    pub agent: AgentFile,
    /// What `graphrt serve` offers besides the API: chat page, monitoring...
    pub serve: ServeOptions,
    /// Delete what the store holds once this deploy, which must not need it, serves.
    pub drop_store: bool,
}

impl std::fmt::Debug for DeployRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployRequest")
            .field("deployment", &self.deployment)
            .field("version", &self.version)
            .field("mode", &self.mode)
            .field("image", &self.image)
            .field("secret_names", &self.secrets.keys().collect::<Vec<_>>())
            .field("agent", &self.agent)
            .field("serve", &self.serve)
            .field("drop_store", &self.drop_store)
            .finish_non_exhaustive()
    }
}

/// The store of a deployment: the `store/` folder of its bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StoreSummary {
    /// Cloud Storage bucket.
    pub bucket: String,
    /// Folder of the store in the bucket.
    pub path: String,
    /// Whether the serving version uses it. `false`: kept from an earlier deploy
    /// (billed by Cloud Storage, cents) until a deploy with `--drop-store` empties it.
    pub in_use: bool,
}

/// What a successful deploy produced.
#[derive(Clone, Serialize)]
pub struct DeployOutcome {
    /// Cloud Run service name.
    pub app: String,
    pub url: String,
    pub version: String,
    /// Workflow name to call: `POST {url}/v1/workflows/{workflow}/runs`.
    pub workflow: String,
    /// The API key, only when this deploy created or rotated it (shown once).
    pub api_key: Option<String>,
    /// What the version serves besides the API.
    pub serve: ServeSummary,
    /// The deployment's store, if it has one.
    pub store: Option<StoreSummary>,
    pub project: String,
    pub region: String,
    /// The image the revision runs.
    pub image: String,
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

/// Overall state of a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    /// A version serves (scale-to-zero: possibly no instance until the next request).
    Online,
    /// Paused: off the internet, no instances.
    Paused,
    /// Cloud Run is rolling out a change.
    Deploying,
    /// The last change failed and no version serves.
    Failed,
    /// No version serves (the first deploy failed).
    Offline,
}

/// Where the traffic of a deployment goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrafficSummary {
    /// Version label of the revision.
    pub version: Option<String>,
    /// Cloud Run revision name.
    pub revision: String,
    /// Share of the requests, 0 to 100.
    pub percent: u32,
}

/// `graphrt deploy status --target cloud-run --direct`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeploymentStatus {
    /// Cloud Run service name.
    pub app: String,
    pub url: String,
    pub status: DeploymentState,
    /// Version label of the revision that serves.
    pub current_version: Option<String>,
    /// Workflow name of the current version.
    pub workflow: Option<String>,
    /// `always-on` or `scale-to-zero`.
    pub mode: Option<String>,
    /// When the current version was deployed (RFC 3339, UTC).
    pub last_deployed_at: Option<String>,
    pub traffic: Vec<TrafficSummary>,
    pub store: Option<StoreSummary>,
    pub project: String,
    pub region: String,
    /// What Cloud Run says about the last change, when it failed.
    pub message: Option<String>,
}

/// One entry of `graphrt deploy list-versions --target cloud-run --direct`, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VersionSummary {
    /// Version label.
    pub label: String,
    pub deployed_at: Option<String>,
    pub current: bool,
    /// Cloud Run revision name.
    pub revision: String,
    pub mode: Option<String>,
    pub workflow: Option<String>,
    /// `serving` or `standby`.
    pub state: String,
}

/// The version that serves after a rollback or a resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServingVersion {
    pub app: String,
    pub url: String,
    pub version: String,
    pub workflow: Option<String>,
}

/// Settings of the Cloud Run direct deployer.
#[derive(Clone, Debug)]
pub struct DirectSettings {
    /// Google Cloud project id.
    pub project: String,
    /// Cloud Run region (`us-central1`, `europe-west1`, `asia-southeast2`...).
    pub region: String,
    /// API base URLs.
    pub endpoints: Endpoints,
    /// Labels added to every resource the deploy creates (`team=sales`).
    pub labels: BTreeMap<String, String>,
    /// Longest wait for a revision to get ready.
    pub start_timeout: Duration,
    /// Longest wait for `GET <url>/readyz` to pass once the traffic moved.
    pub health_timeout: Duration,
    /// Pause between two polls of an operation.
    pub poll_interval: Duration,
    /// Pause before the first retry of a failed call (doubles each time).
    pub retry_base: Duration,
    /// Versions kept for rollback.
    pub keep_versions: usize,
    /// Base URL of the public check instead of the service URL (tests).
    pub public_url_override: Option<String>,
}

impl DirectSettings {
    /// Defaults: 10 min to start, 2 min health, 2 s polls, 5 versions.
    pub fn new(project: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            region: region.into(),
            endpoints: Endpoints::default(),
            labels: BTreeMap::new(),
            start_timeout: Duration::from_secs(600),
            health_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
            retry_base: Duration::from_millis(500),
            keep_versions: DEFAULT_KEEP_VERSIONS,
            public_url_override: None,
        }
    }

    /// Checks ids and endpoints before any call.
    ///
    /// @throws DirectError(InvalidInput) - a project, region or label Google would refuse, a plain http endpoint
    pub fn validate(&self) -> Result<(), DirectError> {
        let project_ok = (6..=30).contains(&self.project.len())
            && self.project.starts_with(|character: char| character.is_ascii_lowercase())
            && self.project.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !project_ok {
            return Err(DirectError::invalid(format!(
                "--project {:?} is not a Google Cloud project id (6 to 30 lowercase letters, digits and dashes; \
                 the id, not the display name: see `gcloud projects list`)",
                self.project
            )));
        }
        let region_ok = !self.region.is_empty()
            && self.region.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && self.region.contains('-');
        if !region_ok {
            return Err(DirectError::invalid(format!(
                "--region {:?} is not a Cloud Run region such as us-central1 or asia-southeast2",
                self.region
            )));
        }
        for endpoint in self.endpoints.all() {
            if !crate::cloud_run::is_allowed_endpoint(endpoint) {
                return Err(DirectError::invalid(format!("endpoint {endpoint} must be https:// (or loopback http)")));
            }
        }
        for (key, value) in &self.labels {
            if !naming::is_label_key(key) || !naming::is_label_value(value) {
                return Err(DirectError::invalid(format!(
                    "--label {key}={value}: keys and values are lowercase letters, digits, `_` and `-` (keys start \
                     with a letter, 63 characters at most)"
                )));
            }
        }
        Ok(())
    }
}

/// The direct deployer of one Cloud Run service.
#[derive(Debug)]
pub struct CloudRunDirect {
    settings: DirectSettings,
    service: String,
    client: google::GoogleClient,
}

impl CloudRunDirect {
    /// A deployer for the service `service` (see [`service_name_for`]).
    ///
    /// @param settings - project, region, endpoints, timeouts (DirectSettings)
    /// @param auth - the person's or the operator's Google credentials (&GcpAuth)
    /// @param service - Cloud Run service name (String)
    /// @throws DirectError(InvalidInput) - a project, region, label or service name Google would refuse
    pub fn new(settings: DirectSettings, auth: &GcpAuth, service: String) -> Result<Self, DirectError> {
        settings.validate()?;
        if !naming::is_service_name(&service) {
            return Err(DirectError::invalid(format!(
                "Cloud Run service name {service:?} must be 1 to 49 lowercase letters, digits and dashes, starting \
                 with a letter"
            )));
        }
        let user_project = auth.kind().bills_quota_to_caller().then(|| settings.project.clone());
        let context = google::Context { project: settings.project.clone(), principal: auth.principal().to_owned() };
        let client = google::GoogleClient::new(
            auth.access_tokens(),
            user_project,
            settings.endpoints.clone(),
            context,
            settings.retry_base,
        )?;
        Ok(Self { settings, service, client })
    }

    /// The Cloud Run service this deployer manages.
    pub fn service_name(&self) -> &str {
        &self.service
    }

    /// Project and region.
    pub fn location(&self) -> (&str, &str) {
        (&self.settings.project, &self.settings.region)
    }
}

/// A progress event.
pub(crate) fn emit(
    progress: &mut dyn FnMut(StepEvent),
    step: graphrt_provider_api::direct::Step,
    status: graphrt_provider_api::direct::StepStatus,
    message: impl Into<String>,
) {
    progress(StepEvent { step, status, message: message.into() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_refuse_display_names_and_bad_regions() {
        assert!(DirectSettings::new("my-project", "us-central1").validate().is_ok());
        assert!(DirectSettings::new("My Project", "us-central1").validate().is_err());
        assert!(DirectSettings::new("my-project", "us central").validate().is_err());
        let mut labelled = DirectSettings::new("my-project", "us-central1");
        labelled.labels.insert("graphrt-test".into(), "1".into());
        assert!(labelled.validate().is_ok());
        labelled.labels.insert("Team".into(), "x".into());
        assert!(labelled.validate().is_err());
        let mut plain = DirectSettings::new("my-project", "us-central1");
        plain.endpoints.run = "http://example.com".into();
        assert!(plain.validate().is_err());
    }

    #[test]
    fn debug_output_never_shows_secret_values_or_the_key() {
        let request = DeployRequest {
            deployment: "d".into(),
            version: "v1".into(),
            mode: ServiceMode::ScaleToZero,
            image: None,
            secrets: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-very-secret".into())]),
            api_key_candidate: "grk_candidate_value".into(),
            rotate_api_key: false,
            agent: AgentFile::new("a.yaml", b"x".to_vec()),
            serve: ServeOptions::default(),
            drop_store: false,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("sk-very-secret") && !debug.contains("grk_candidate_value"), "{debug}");
    }
}
