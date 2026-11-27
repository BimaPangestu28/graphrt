//! Operator configuration of the Cloud Run provider. Endpoints come from here,
//! never from a deployment spec (SSRF, plan §6).

use std::time::Duration;

use graphrt_provider_api::ProviderError;

/// Cloud Run Admin API v2.
pub const DEFAULT_ADMIN_ENDPOINT: &str = "https://run.googleapis.com";
/// Secret Manager API v1.
pub const DEFAULT_SECRET_MANAGER_ENDPOINT: &str = "https://secretmanager.googleapis.com";
/// Cloud Run's hard cap on one request (and so on one SSE stream): 60 minutes.
pub const MAX_REQUEST_TIMEOUT_SECS: u64 = 3_600;

/// How the gateway authenticates its calls to the Cloud Run service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeAuth {
    /// A Google-signed ID token in `X-Serverless-Authorization` (IAM
    /// `roles/run.invoker`). Cloud Run checks and strips it before the
    /// container, so it never collides with the agent's own auth.
    IdToken,
    /// No platform auth (service deployed with `invokerIamDisabled`); only the
    /// `Graphrt-Sig` HMAC protects the agent. For private networks only.
    None,
}

/// Configuration of [`super::CloudRunProvider`] and [`super::CloudRunDeployDriver`].
#[derive(Debug, Clone)]
pub struct CloudRunConfig {
    /// GCP project id that hosts the services.
    pub project: String,
    /// Region (`europe-west1`).
    pub region: String,
    /// Cloud Run Admin API base URL.
    pub admin_endpoint: String,
    /// Secret Manager base URL.
    pub secret_manager_endpoint: String,
    /// Service id prefix: deployment `chat` becomes service `graphrt-chat`.
    pub service_prefix: String,
    /// Request timeout set on revisions (≤ 3600 s). The gateway preempts runs at
    /// this cap minus 60 s and re-dispatches them.
    pub request_timeout_secs: u64,
    /// Upper bound of one wake (cold start + first `/readyz`).
    pub wake_timeout: Duration,
    /// A deployment probed this recently is assumed warm and not probed again.
    pub warm_window: Duration,
    /// How long a resolved service URL is reused before `services.get` again.
    pub url_cache_ttl: Duration,
    /// Token bucket of the Admin API (requests per second, burst).
    pub admin_requests_per_second: f64,
    /// Burst of the Admin API token bucket.
    pub admin_burst: u32,
    /// Platform auth of invocations.
    pub invoke_auth: InvokeAuth,
    /// Longest wait for a deploy's long-running operation.
    pub deploy_timeout: Duration,
    /// Poll interval of long-running operations.
    pub operation_poll_interval: Duration,
}

impl CloudRunConfig {
    /// Defaults for `project` and `region`.
    pub fn new(project: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            region: region.into(),
            admin_endpoint: DEFAULT_ADMIN_ENDPOINT.to_owned(),
            secret_manager_endpoint: DEFAULT_SECRET_MANAGER_ENDPOINT.to_owned(),
            service_prefix: "graphrt-".to_owned(),
            request_timeout_secs: MAX_REQUEST_TIMEOUT_SECS,
            wake_timeout: Duration::from_secs(30),
            warm_window: Duration::from_secs(5),
            url_cache_ttl: Duration::from_secs(60),
            // Cloud Run Admin API read quota is per project and region; stay far below it.
            admin_requests_per_second: 5.0,
            admin_burst: 10,
            invoke_auth: InvokeAuth::IdToken,
            deploy_timeout: Duration::from_secs(600),
            operation_poll_interval: Duration::from_secs(2),
        }
    }

    /// Checks the configuration at gateway startup.
    ///
    /// @throws `ProviderError` (fatal) on an invalid project/region, an endpoint
    ///   that is neither HTTPS nor loopback HTTP, or a timeout above 3600 s.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let valid_id = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-:.".contains(&byte))
        };
        if !valid_id(&self.project) {
            return Err(ProviderError::fatal(format!("invalid GCP project id `{}`", self.project)));
        }
        if !valid_id(&self.region) || self.region.contains([':', '.']) {
            return Err(ProviderError::fatal(format!("invalid Cloud Run region `{}`", self.region)));
        }
        for endpoint in [&self.admin_endpoint, &self.secret_manager_endpoint] {
            if !is_allowed_endpoint(endpoint) {
                return Err(ProviderError::fatal(format!("endpoint `{endpoint}` must be https:// (or loopback http)")));
            }
        }
        if self.request_timeout_secs == 0 || self.request_timeout_secs > MAX_REQUEST_TIMEOUT_SECS {
            return Err(ProviderError::fatal("request_timeout_secs must be within 1..=3600 (Cloud Run maximum)"));
        }
        if self.admin_requests_per_second <= 0.0 || self.admin_burst == 0 {
            return Err(ProviderError::fatal("the Admin API token bucket needs a positive rate and burst"));
        }
        Ok(())
    }

    /// `projects/{project}/locations/{region}`.
    pub fn parent(&self) -> String {
        format!("projects/{}/locations/{}", self.project, self.region)
    }
}

/// HTTPS, or plain HTTP to a loopback address (fake servers, local emulators).
pub(crate) fn is_allowed_endpoint(endpoint: &str) -> bool {
    if endpoint.starts_with("https://") {
        return true;
    }
    let Some(rest) = endpoint.strip_prefix("http://") else { return false };
    if rest.starts_with("[::1]") {
        return true;
    }
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    matches!(host, "127.0.0.1" | "localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        CloudRunConfig::new("my-project", "europe-west1").validate().unwrap();
    }

    #[test]
    fn rejects_remote_plain_http_and_bad_ids() {
        let mut config = CloudRunConfig::new("my-project", "europe-west1");
        config.admin_endpoint = "http://169.254.169.254".to_owned();
        assert!(config.validate().is_err());
        config.admin_endpoint = "http://127.0.0.1:9000".to_owned();
        config.validate().unwrap();
        assert!(CloudRunConfig::new("My Project", "europe-west1").validate().is_err());
        assert!(CloudRunConfig::new("p", "europe/west1").validate().is_err());
        let mut long = CloudRunConfig::new("p", "r");
        long.request_timeout_secs = 3_601;
        assert!(long.validate().is_err());
    }
}
