//! Direct deploys to Fly.io: an agent served straight by the Fly proxy at
//! `https://<app>.fly.dev`, with no graphrt gateway (`graphrt deploy --target
//! fly --direct`, see `docs/deploy-direct.md`).
//!
//! One Fly app per deployment. Each deployment **version** is one Fly Machine
//! running the published graphrt image with `graphrt serve /workflows`; the
//! agent (a `.grt` bundle or a workflow file) is written into the Machine by
//! Fly from `config.files`, so no image is built or pushed. The Machine's role
//! lives in its metadata:
//!
//! | role | services | state |
//! | --- | --- | --- |
//! | `candidate` | public (80/443) | being started and health-checked |
//! | `current` | public | serving (always-on: kept running; scale-to-zero: Fly proxy autostop/autostart) |
//! | `standby` | none | stopped, kept for rollback (only the rootfs is billed) |
//!
//! A deploy creates (or updates) the candidate, waits for its `/readyz`
//! readiness check to pass, then promotes it and demotes the previous current
//! Machine to standby. A candidate that never gets healthy is destroyed and
//! the previous version keeps serving. Standby Machines have no services, so
//! the proxy never routes to (or autostarts) an old version.
//!
//! A deploy that turns on monitoring, devices, push notifications,
//! conversations or a messaging channel ([`ServeOptions::needs_store`]) keeps state that must outlive
//! a Machine stop and a redeploy. It gets one 1 GB Fly volume as its `--store`.
//! Fly attaches a volume to a Machine only when the Machine is created, so the
//! Machine with the volume is **updated in place** by every later deploy that
//! needs the store (a short restart, and no standby version of it to roll back
//! to); a version that does not get healthy is replaced by the config it had.
//! Deploys that need no store never create a volume.
//!
//! Secrets are Fly app secrets (injected as environment variables at boot);
//! their values are only ever sent in the body of `POST /apps/{app}/secrets`.
//! A channel's credentials are such secrets too ([`DirectChannel::secret_names`]):
//! the Machine command names only the channel's kind and workflow.

mod lifecycle;
mod machine_config;
mod serve_options;
mod status;

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use serde::Serialize;

use crate::client::{ClientPolicy, MachinesClient, PUBLIC_API_BASE_URL};
use crate::token::TokenOrigin;

pub use lifecycle::{DeployFailure, Progress, ServingVersion};
pub use machine_config::{AGENT_PORT, MAX_AGENT_FILE_BYTES, WORKFLOW_DIR, app_name_for, workflow_name_of};
pub use serve_options::{
    ChatAccess, ChatOptions, DirectChannel, STORE_PATH, STORE_VOLUME_GB, STORE_VOLUME_NAME, ServeOptions, ServeSummary,
    StoreSummary,
};
pub use status::{DeploymentState, DeploymentStatus, MachineSummary, VersionSummary, format_rfc3339_ms};

/// Name of the Fly secret holding the deployment's API key (`graphrt serve` reads it).
pub const API_KEY_SECRET: &str = "GRAPHRT_API_KEY";
/// Standby versions kept for rollback; older ones are destroyed after a deploy.
pub const DEFAULT_KEEP_VERSIONS: usize = 5;

/// How the deployment's Machine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceMode {
    /// One Machine always running (`autostop = off`, `min_machines_running = 1`).
    AlwaysOn,
    /// Fly proxy stops the Machine when idle and starts it on the next request.
    ScaleToZero,
}

impl ServiceMode {
    /// The name used on the command line and in metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysOn => "always-on",
            Self::ScaleToZero => "scale-to-zero",
        }
    }

    /// Parses [`ServiceMode::as_str`].
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "always-on" => Some(Self::AlwaysOn),
            "scale-to-zero" => Some(Self::ScaleToZero),
            _ => None,
        }
    }
}

/// The agent shipped to the Machine.
#[derive(Clone)]
pub struct AgentFile {
    /// File name under [`WORKFLOW_DIR`], e.g. `support.grt` or `support.yaml`;
    /// `graphrt serve` names the workflow after its stem.
    pub file_name: String,
    /// File contents (at most [`MAX_AGENT_FILE_BYTES`]).
    pub contents: Vec<u8>,
}

impl std::fmt::Debug for AgentFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentFile")
            .field("file_name", &self.file_name)
            .field("bytes", &self.contents.len())
            .finish()
    }
}

/// One `graphrt deploy --direct` call.
#[derive(Clone)]
pub struct DeployRequest {
    /// Deployment name (Machine metadata, `GRAPHRT_DEPLOYMENT`).
    pub deployment: String,
    /// Version label of this deploy.
    pub version: String,
    /// Fly region of the Machine; empty = Fly picks the closest.
    pub region: String,
    /// Always-on or scale-to-zero.
    pub mode: ServiceMode,
    /// Organization slug, used only when the app has to be created.
    pub org_slug: String,
    /// Container image running `graphrt serve`.
    pub image: String,
    /// Fly secrets to write: name → value (never logged or returned).
    pub secrets: BTreeMap<String, String>,
    /// A freshly generated API key, stored as [`API_KEY_SECRET`] when the app
    /// has none yet or `rotate_api_key` is set (and then returned once).
    pub api_key_candidate: String,
    /// Replace an existing API key.
    pub rotate_api_key: bool,
    /// The agent.
    pub agent: AgentFile,
    /// Wall clock of the deploy (Unix ms), recorded as the version's deploy time.
    pub now_ms: u64,
    /// What `graphrt serve` offers besides the API: chat page, monitoring...
    pub serve: ServeOptions,
    /// Delete the store volume (and what is on it) once this deploy, which
    /// must not need it, serves.
    pub drop_store: bool,
}

impl std::fmt::Debug for DeployRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployRequest")
            .field("deployment", &self.deployment)
            .field("version", &self.version)
            .field("region", &self.region)
            .field("mode", &self.mode)
            .field("image", &self.image)
            .field("secret_names", &self.secrets.keys().collect::<Vec<_>>())
            .field("agent", &self.agent)
            .field("serve", &self.serve)
            .field("drop_store", &self.drop_store)
            .finish_non_exhaustive()
    }
}

/// What a successful deploy produced.
#[derive(Clone, Serialize)]
pub struct DeployOutcome {
    pub app: String,
    pub url: String,
    pub version: String,
    /// Workflow name to call: `POST {url}/v1/workflows/{workflow}/runs`.
    pub workflow: String,
    /// The API key, only when this deploy created or rotated it (shown once).
    pub api_key: Option<String>,
    /// What the version serves besides the API.
    pub serve: ServeSummary,
    /// The deployment's store volume, if it has one (billed by Fly while it exists).
    pub store: Option<StoreSummary>,
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

/// The phases a deploy reports progress for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Input checks, the app and its public IPs.
    Preparing,
    /// Fly secrets and the API key.
    Secrets,
    /// Creating or updating the version's Machine and booting it.
    Starting,
    /// Waiting for `/readyz`, then promoting the version.
    Checking,
}

/// State of a step in a progress event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Done,
    Failed,
}

/// One progress event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepEvent {
    pub step: Step,
    pub status: StepStatus,
    pub message: String,
}

/// Stable error codes of the direct deployer (the `code` of a JSON `error` line).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A flag or the agent file is invalid.
    InvalidInput,
    /// The Fly token is missing, invalid or lacks a permission.
    Unauthorized,
    /// Another organization owns the app name.
    AppNameTaken,
    /// The app, version or Machine does not exist.
    NotFound,
    /// Fly has no capacity in the region.
    Capacity,
    /// Fly keeps rate limiting the calls.
    RateLimited,
    /// The Machine did not boot in time.
    StartTimeout,
    /// `/readyz` never passed; the previous version (if any) keeps serving.
    HealthCheckFailed,
    /// Fly is unreachable or answered 5xx.
    FlyUnavailable,
    /// Fly refused a call (4xx).
    FlyApi,
}

impl ErrorCode {
    /// The snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Unauthorized => "unauthorized",
            Self::AppNameTaken => "app_name_taken",
            Self::NotFound => "not_found",
            Self::Capacity => "capacity",
            Self::RateLimited => "rate_limited",
            Self::StartTimeout => "start_timeout",
            Self::HealthCheckFailed => "health_check_failed",
            Self::FlyUnavailable => "fly_unavailable",
            Self::FlyApi => "fly_api",
        }
    }
}

/// A failed direct-deploy operation. Messages never contain the token or a secret value.
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

impl From<ProviderError> for DirectError {
    fn from(error: ProviderError) -> Self {
        let code = match error.kind {
            ProviderErrorKind::Auth => ErrorCode::Unauthorized,
            ProviderErrorKind::NotFound => ErrorCode::NotFound,
            ProviderErrorKind::Capacity => ErrorCode::Capacity,
            ProviderErrorKind::RateLimited => ErrorCode::RateLimited,
            ProviderErrorKind::Transient => ErrorCode::FlyUnavailable,
            _ => ErrorCode::FlyApi,
        };
        Self::new(code, error.message)
    }
}

/// Settings of the direct deployer.
#[derive(Clone, Debug)]
pub struct DirectSettings {
    /// Machines API base URL ([`PUBLIC_API_BASE_URL`]).
    pub api_base_url: String,
    /// Where the Fly token is read from (`FLY_API_TOKEN`).
    pub token_origin: TokenOrigin,
    /// Retries and rate limit.
    pub client_policy: ClientPolicy,
    /// Longest wait for a Machine to boot.
    pub start_timeout: Duration,
    /// Longest wait for the `/readyz` check to pass after boot.
    pub health_timeout: Duration,
    /// Pause between two health polls.
    pub poll_interval: Duration,
    /// Standby versions kept for rollback.
    pub keep_versions: usize,
}

impl DirectSettings {
    /// Defaults for the public API: 2 min boot, 2 min health, 2 s polls, 5 versions.
    pub fn new(token_origin: TokenOrigin) -> Self {
        Self {
            api_base_url: PUBLIC_API_BASE_URL.to_owned(),
            token_origin,
            client_policy: ClientPolicy::default(),
            start_timeout: Duration::from_secs(120),
            health_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
            keep_versions: DEFAULT_KEEP_VERSIONS,
        }
    }
}

/// The direct deployer of one Fly app.
#[derive(Debug)]
pub struct FlyDirect {
    settings: DirectSettings,
    client: MachinesClient,
}

impl FlyDirect {
    /// A deployer for `app_name` (see [`app_name_for`]).
    ///
    /// @param settings - API, token and timeouts (DirectSettings)
    /// @param app_name - Fly app name (String)
    /// @returns The deployer
    /// @throws DirectError(InvalidInput) - an app name Fly would refuse
    pub fn new(settings: DirectSettings, app_name: String) -> Result<Self, DirectError> {
        let client = MachinesClient::new(
            settings.api_base_url.clone(),
            app_name,
            settings.token_origin.clone(),
            settings.client_policy.clone(),
        )
        .map_err(|error| DirectError::invalid(error.message))?;
        Ok(Self { settings, client })
    }

    /// The Fly app this deployer manages.
    pub fn app_name(&self) -> &str {
        self.client.app_name()
    }

    /// `https://<app>.fly.dev`.
    pub fn url(&self) -> String {
        public_url(self.app_name())
    }
}

/// The public URL Fly gives an app with public IPs.
pub fn public_url(app_name: &str) -> String {
    format!("https://{app_name}.fly.dev")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip() {
        for mode in [ServiceMode::AlwaysOn, ServiceMode::ScaleToZero] {
            assert_eq!(ServiceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ServiceMode::parse("sometimes"), None);
    }

    #[test]
    fn provider_errors_map_to_stable_codes() {
        assert_eq!(DirectError::from(ProviderError::auth("no")).code, ErrorCode::Unauthorized);
        assert_eq!(DirectError::from(ProviderError::fatal("bad")).code, ErrorCode::FlyApi);
        assert_eq!(DirectError::from(ProviderError::transient("5xx")).code, ErrorCode::FlyUnavailable);
    }

    #[test]
    fn debug_output_never_shows_secret_values_or_the_key() {
        let request = DeployRequest {
            deployment: "d".into(),
            version: "v1".into(),
            region: String::new(),
            mode: ServiceMode::ScaleToZero,
            org_slug: "personal".into(),
            image: "img".into(),
            secrets: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-very-secret".into())]),
            api_key_candidate: "grk_candidate_value".into(),
            rotate_api_key: false,
            agent: AgentFile { file_name: "a.yaml".into(), contents: b"x".to_vec() },
            now_ms: 0,
            serve: ServeOptions::default(),
            drop_store: false,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("sk-very-secret") && !debug.contains("grk_candidate_value"), "{debug}");
        let outcome = DeployOutcome {
            app: "a".into(),
            url: "u".into(),
            version: "v".into(),
            workflow: "w".into(),
            api_key: Some("grk_shown_once".into()),
            serve: ServeOptions::default().summary(),
            store: None,
        };
        assert!(!format!("{outcome:?}").contains("grk_shown_once"));
    }
}
