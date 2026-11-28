//! Deploy, status, versions, rollback, pause, resume and remove of a direct
//! Lambda deployment.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};

use super::layer::{Entry, build, extract_file};
use super::registry::{Ecr, ImageRef, RegistryReader, config_with_layer, manifest_with_layer};
use super::{
    AGENT_PORT, API_KEY_ENV, DeployOutcome, DeployRequest, DirectError, ErrorCode, FUNCTION_TIMEOUT_SECS, LIVE_ALIAS,
    LambdaDirect, Step, StepEvent, StepStatus, WORKFLOW_DIR, account,
};
use crate::api::{DeploymentRef, ProviderErrorKind};
use crate::auth::sigv4::uri_encode;
use crate::lambda::lwa::AgentRequest;
use crate::lambda::{InvokeError, LambdaConfig, LambdaProvider};
use crate::rest::is_conflict;

/// Progress callback of a deploy, rollback or resume.
pub type Progress<'a> = &'a mut (dyn FnMut(StepEvent) + Send);

/// Environment names the deployer manages itself (not carried over as secrets).
const MANAGED_ENV: [&str; 7] = [
    "GRAPHRT_DEPLOYMENT",
    "GRAPHRT_DEPLOYMENT_VERSION",
    "AWS_LWA_PORT",
    "AWS_LWA_INVOKE_MODE",
    "AWS_LWA_READINESS_CHECK_PATH",
    "HOME",
    "TMPDIR",
];
/// Lambda's limit on the environment (names and values).
const MAX_ENVIRONMENT_BYTES: usize = 4 * 1024;
/// Path of the adapter binary inside its image.
const ADAPTER_BINARY: &str = "lambda-adapter";
const DESCRIPTION_PREFIX: &str = "graphrt";
/// Function errors in a row after which a new version counts as broken.
const MAX_START_CRASHES: u32 = 3;

/// A failed deploy. `api_key` is set when a new key was already stored in the
/// function, so the caller can still hand it over once.
#[derive(Clone, Debug)]
pub struct DeployFailure {
    pub error: DirectError,
    pub api_key: Option<String>,
}

impl From<DirectError> for DeployFailure {
    fn from(error: DirectError) -> Self {
        Self { error, api_key: None }
    }
}

/// The version that serves after a rollback or resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServingVersion {
    pub app: String,
    pub region: String,
    pub url: String,
    pub version: String,
    pub workflow: Option<String>,
}

/// Overall state of a deployment (same names as the Fly deployer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    /// `live` serves (no instance runs until a request comes).
    Online,
    /// Reserved concurrency 0: every request is refused with 429.
    Paused,
    /// The function is being updated.
    Deploying,
    /// The function is in a failed state.
    Failed,
    /// The function exists but nothing serves (its first deploy failed).
    Offline,
}

/// `graphrt deploy status --target lambda --direct`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeploymentStatus {
    pub app: String,
    pub region: String,
    pub url: String,
    pub status: DeploymentState,
    pub current_version: Option<String>,
    pub workflow: Option<String>,
    /// Always `scale-to-zero`.
    pub mode: Option<String>,
    pub last_deployed_at: Option<String>,
    /// Lambda has no Machines: always empty (kept for the shape the Fly status has).
    pub machines: Vec<Value>,
    pub store: Option<()>,
}

/// One entry of `list-versions`, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VersionSummary {
    pub label: String,
    pub deployed_at: Option<String>,
    pub current: bool,
    /// `current` or `standby`.
    pub role: String,
    pub mode: Option<String>,
    pub workflow: Option<String>,
    /// `active` (Lambda versions never stop).
    pub state: String,
    /// The Lambda version number serving this label.
    pub lambda_version: String,
}

/// One published Lambda version made by a direct deploy.
#[derive(Clone, Debug)]
struct PublishedVersion {
    number: u64,
    label: String,
    workflow: String,
    deployed_ms: u64,
    environment: BTreeMap<String, String>,
}

/// `graphrt v=<label> w=<workflow> t=<unix ms>`.
fn version_description(label: &str, workflow: &str, deployed_ms: u64) -> String {
    format!("{DESCRIPTION_PREFIX} v={label} w={workflow} t={deployed_ms}")
}

fn parse_description(description: &str) -> Option<(String, String, u64)> {
    let rest = description.strip_prefix(DESCRIPTION_PREFIX)?.trim();
    let mut label = None;
    let mut workflow = None;
    let mut deployed = None;
    for part in rest.split(' ') {
        if let Some(value) = part.strip_prefix("v=") {
            label = Some(value.to_owned());
        } else if let Some(value) = part.strip_prefix("w=") {
            workflow = Some(value.to_owned());
        } else if let Some(value) = part.strip_prefix("t=") {
            deployed = value.parse().ok();
        }
    }
    Some((label?, workflow.unwrap_or_default(), deployed.unwrap_or(0)))
}

/// Whether `label` can be a version label here (it is an image tag too).
pub(crate) fn is_version_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && label.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

fn environment_of(configuration: &Value) -> BTreeMap<String, String> {
    configuration
        .get("Environment")
        .and_then(|environment| environment.get("Variables"))
        .and_then(Value::as_object)
        .map(|variables| {
            variables.iter().filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned()))).collect()
        })
        .unwrap_or_default()
}

/// The carried-over part of an environment: secrets and the API key.
fn secrets_of(environment: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    environment
        .iter()
        .filter(|(key, _)| !MANAGED_ENV.contains(&key.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// RFC 3339 (UTC, milliseconds) of a Unix time in ms, as the Fly deployer writes it.
fn rfc3339(unix_ms: u64) -> String {
    let seconds = unix_ms / 1000;
    let days = seconds / 86_400;
    let time = seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60,
        unix_ms % 1000
    )
}

fn function_path(function: &str, suffix: &str) -> String {
    format!("/2015-03-31/functions/{}{suffix}", uri_encode(function, false))
}

fn event(progress: &mut Progress<'_>, step: Step, status: StepStatus, message: impl Into<String>) {
    progress(StepEvent { step, status, message: message.into() });
}

/// Memory of the function. `graphrt serve` with a workflow uses about 30 MB, so
/// 512 MB is plenty (Lambda bills by memory times time); a big bundle needs room
/// once unpacked.
fn memory_mib(unpacked_bytes: u64) -> u64 {
    match unpacked_bytes {
        bytes if bytes > 256 * 1024 * 1024 => 3008,
        bytes if bytes > 32 * 1024 * 1024 => 2048,
        _ => 512,
    }
}

fn lambda_architecture(oci: &str) -> &'static str {
    if oci == "arm64" { "arm64" } else { "x86_64" }
}

impl LambdaDirect {
    async fn get_function(&self, qualifier: Option<&str>) -> Result<Option<Value>, DirectError> {
        let query: Vec<(&str, &str)> = qualifier.map(|q| vec![("Qualifier", q)]).unwrap_or_default();
        match self.lambda.json_rest("GET", &function_path(&self.function, ""), &query, None).await {
            Ok(function) => Ok(Some(function)),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(error) => Err(DirectError::from_aws(error, "read the Lambda function")),
        }
    }

    async fn require_function(&self) -> Result<Value, DirectError> {
        self.get_function(None).await?.ok_or_else(|| {
            DirectError::new(
                ErrorCode::NotFound,
                format!(
                    "there is no deployment {} in {} (check --deployment and --region)",
                    self.function, self.settings.region
                ),
            )
        })
    }

    async fn versions(&self) -> Result<Vec<PublishedVersion>, DirectError> {
        let mut versions = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut query = vec![("MaxItems", "50")];
            if let Some(marker) = &marker {
                query.push(("Marker", marker.as_str()));
            }
            let page = self
                .lambda
                .json_rest("GET", &function_path(&self.function, "/versions"), &query, None)
                .await
                .map_err(|error| DirectError::from_aws(error, "list the function's versions"))?;
            for configuration in page.get("Versions").and_then(Value::as_array).into_iter().flatten() {
                let Some(number) = configuration.get("Version").and_then(Value::as_str).and_then(|v| v.parse().ok())
                else {
                    continue;
                };
                let description = configuration.get("Description").and_then(Value::as_str).unwrap_or_default();
                let Some((label, workflow, deployed_ms)) = parse_description(description) else { continue };
                versions.push(PublishedVersion {
                    number,
                    label,
                    workflow,
                    deployed_ms,
                    environment: environment_of(configuration),
                });
            }
            marker = page.get("NextMarker").and_then(Value::as_str).map(str::to_owned);
            if marker.is_none() {
                break;
            }
        }
        versions.sort_by_key(|version| std::cmp::Reverse(version.number));
        Ok(versions)
    }

    async fn live_version(&self) -> Result<Option<u64>, DirectError> {
        let path = function_path(&self.function, &format!("/aliases/{LIVE_ALIAS}"));
        match self.lambda.json_rest("GET", &path, &[], None).await {
            Ok(alias) => Ok(alias.get("FunctionVersion").and_then(Value::as_str).and_then(|v| v.parse().ok())),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(error) => Err(DirectError::from_aws(error, "read the live alias")),
        }
    }

    async fn point_live(&self, number: u64) -> Result<(), DirectError> {
        let version = number.to_string();
        let path = function_path(&self.function, &format!("/aliases/{LIVE_ALIAS}"));
        let body = json!({ "FunctionVersion": version });
        match self.lambda.json_rest("PUT", &path, &[], Some(&body)).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind == ProviderErrorKind::NotFound => {
                let create = json!({ "Name": LIVE_ALIAS, "FunctionVersion": version });
                self.lambda
                    .json_rest("POST", &function_path(&self.function, "/aliases"), &[], Some(&create))
                    .await
                    .map(|_| ())
                    .map_err(|error| DirectError::from_aws(error, "point the live alias at the new version"))
            }
            Err(error) => Err(DirectError::from_aws(error, "point the live alias at the new version")),
        }
    }

    async fn function_url(&self) -> Result<Option<String>, DirectError> {
        let path = format!("/2021-10-31/functions/{}/url", uri_encode(&self.function, false));
        match self.lambda.json_rest("GET", &path, &[("Qualifier", LIVE_ALIAS)], None).await {
            Ok(config) => {
                Ok(config.get("FunctionUrl").and_then(Value::as_str).map(|url| url.trim_end_matches('/').to_owned()))
            }
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(error) => Err(DirectError::from_aws(error, "read the function URL")),
        }
    }

    /// The public URL of `live` (created once) and the permissions that let anyone call it.
    async fn ensure_function_url(&self) -> Result<String, DirectError> {
        let context = "create the public function URL";
        let url = match self.function_url().await? {
            Some(url) => url,
            None => {
                let path = format!("/2021-10-31/functions/{}/url", uri_encode(&self.function, false));
                let body = json!({ "AuthType": "NONE", "InvokeMode": "RESPONSE_STREAM" });
                let created =
                    match self.lambda.json_rest("POST", &path, &[("Qualifier", LIVE_ALIAS)], Some(&body)).await {
                        Ok(created) => created,
                        Err(error) if is_conflict(&error) => self
                            .lambda
                            .json_rest("GET", &path, &[("Qualifier", LIVE_ALIAS)], None)
                            .await
                            .map_err(|e| DirectError::from_aws(e, context))?,
                        Err(error) => return Err(DirectError::from_aws(error, context)),
                    };
                created
                    .get("FunctionUrl")
                    .and_then(Value::as_str)
                    .map(|url| url.trim_end_matches('/').to_owned())
                    .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "Lambda gave no function URL"))?
            }
        };
        let statements = [
            json!({
                "StatementId": "graphrt-public-url", "Action": "lambda:InvokeFunctionUrl",
                "Principal": "*", "FunctionUrlAuthType": "NONE",
            }),
            json!({
                "StatementId": "graphrt-public-url-invoke", "Action": "lambda:InvokeFunction",
                "Principal": "*", "InvokedViaFunctionUrl": true,
            }),
        ];
        for statement in statements {
            let path = function_path(&self.function, "/policy");
            match self.lambda.json_rest("POST", &path, &[("Qualifier", LIVE_ALIAS)], Some(&statement)).await {
                Ok(_) => {}
                Err(error) if is_conflict(&error) => {}
                Err(error) => return Err(DirectError::from_aws(error, "let the public call the function URL")),
            }
        }
        Ok(url)
    }

    async fn reserved_concurrency(&self) -> Result<Option<i64>, DirectError> {
        let path = format!("/2019-09-30/functions/{}/concurrency", uri_encode(&self.function, false));
        match self.lambda.json_rest("GET", &path, &[], None).await {
            Ok(value) => Ok(value.get("ReservedConcurrentExecutions").and_then(Value::as_i64)),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(error) => Err(DirectError::from_aws(error, "read the function's concurrency")),
        }
    }

    /// Waits until no update of the function is in progress. A failed update
    /// left by an earlier deploy does not stop the next one: it is only an error
    /// right after this deploy's own update (`after_update`).
    async fn wait_ready(&self) -> Result<Value, DirectError> {
        self.wait_settled(true).await
    }

    async fn wait_settled(&self, after_update: bool) -> Result<Value, DirectError> {
        let started = Instant::now();
        loop {
            let configuration = self
                .lambda
                .json_rest("GET", &function_path(&self.function, "/configuration"), &[], None)
                .await
                .map_err(|error| DirectError::from_aws(error, "read the function"))?;
            let state = configuration.get("State").and_then(Value::as_str).unwrap_or("Active");
            let update = configuration.get("LastUpdateStatus").and_then(Value::as_str).unwrap_or("Successful");
            if !after_update && state != "Pending" && update != "InProgress" {
                return Ok(configuration);
            }
            if state == "Failed" || update == "Failed" {
                let reason = configuration
                    .get("LastUpdateStatusReason")
                    .or_else(|| configuration.get("StateReason"))
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                let hint = if reason.contains("CustomerError")
                    || reason.contains("ImageConfig")
                    || reason.contains("image")
                {
                    ". The image cannot run here: --image must be a graphrt image (default ghcr.io/graphrt/graphrt:<version>)"
                } else {
                    ""
                };
                return Err(DirectError::new(
                    ErrorCode::AwsApi,
                    format!("Lambda could not prepare the function: {reason}{hint}"),
                ));
            }
            if state == "Active" && update == "Successful" {
                return Ok(configuration);
            }
            if started.elapsed() >= self.settings.ready_timeout {
                return Err(DirectError::new(
                    ErrorCode::AwsUnavailable,
                    format!(
                        "the function was not ready after {} s (state {state}, update {update})",
                        self.settings.ready_timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(self.settings.poll_interval).await;
        }
    }

    /// Copies the graphrt image into ECR with the adapter and the agent; returns
    /// the image URI (by digest) and the Lambda architecture.
    async fn push_image(&self, request: &DeployRequest, account: &str) -> Result<(String, String), DirectError> {
        let ecr = Ecr { client: self.ecr.clone() };
        let base_ref = ImageRef::parse(&request.image)?;
        let base_authorization = if base_ref.is_private_ecr() { Some(ecr.authorization().await?) } else { None };
        let mut base_reader = RegistryReader::new(base_ref, base_authorization)?;
        let base = base_reader.pull(&["arm64", "amd64"]).await?;
        let mut adapter_reader = RegistryReader::new(ImageRef::parse(&request.adapter_image)?, None)?;
        let adapter = adapter_reader.pull(&[base.architecture.as_str()]).await?;
        let adapter_layer = adapter
            .layers
            .last()
            .ok_or_else(|| DirectError::new(ErrorCode::ImageUnavailable, "the adapter image has no layers"))?;
        let adapter_bytes = adapter_reader.layer_blob(&adapter_layer.digest).await?;
        let binary = extract_file(&adapter_bytes, ADAPTER_BINARY)
            .map_err(|message| DirectError::new(ErrorCode::ImageUnavailable, format!("adapter image: {message}")))?;
        let agent_path = format!("{}/{}", WORKFLOW_DIR.trim_start_matches('/'), agent_file_name(request));
        let extra = build(&[
            Entry::dir("opt"),
            Entry::dir("opt/extensions"),
            Entry::file("opt/extensions/lambda-adapter", 0o755, binary),
            Entry::dir(WORKFLOW_DIR.trim_start_matches('/')),
            Entry::file(&agent_path, 0o444, request.agent.contents.clone()),
        ])
        .map_err(|message| DirectError::new(ErrorCode::Internal, message))?;
        let repository = self.repository();
        ecr.ensure_repository(&repository, &[("graphrt:deployment", request.deployment.as_str())]).await?;
        let config = config_with_layer(&base.config, &extra, &format!("graphrt deploy: {}", request.agent.file_name));
        let config_bytes = serde_json::to_vec(&config).map_err(|error| {
            DirectError::new(ErrorCode::Internal, format!("cannot encode the image config: {error}"))
        })?;
        let config_digest = super::layer::sha256_digest(&config_bytes);
        let mut wanted: Vec<String> = base.layers.iter().map(|layer| layer.digest.clone()).collect();
        wanted.push(extra.digest.clone());
        wanted.push(config_digest.clone());
        let missing = ecr.missing_layers(&repository, &wanted).await?;
        for layer in &base.layers {
            if missing.contains(&layer.digest) {
                let bytes = base_reader.layer_blob(&layer.digest).await?;
                ecr.upload(&repository, &layer.digest, &bytes).await?;
            }
        }
        if missing.contains(&extra.digest) {
            ecr.upload(&repository, &extra.digest, &extra.gzip).await?;
        }
        if missing.contains(&config_digest) {
            ecr.upload(&repository, &config_digest, &config_bytes).await?;
        }
        let manifest = manifest_with_layer(&base, &config_bytes, &extra);
        let media_type = manifest.get("mediaType").and_then(Value::as_str).unwrap_or_default().to_owned();
        let manifest_text = serde_json::to_string(&manifest)
            .map_err(|error| DirectError::new(ErrorCode::Internal, format!("cannot encode the manifest: {error}")))?;
        let digest = ecr.put_image(&repository, &request.version, &manifest_text, &media_type).await?;
        let uri = format!("{account}.dkr.ecr.{}.amazonaws.com/{repository}@{digest}", self.settings.region);
        Ok((uri, lambda_architecture(&base.architecture).to_owned()))
    }

    /// Invokes version `number` with `GET /readyz` until it answers 200.
    async fn check_health(&self, number: u64) -> Result<(), DirectError> {
        let config = LambdaConfig {
            endpoint: self.settings.lambda.clone(),
            limits: self.settings.limits,
            ..LambdaConfig::new(&self.settings.region)
        };
        let provider = LambdaProvider::new(std::sync::Arc::clone(&self.chain), config)
            .map_err(|error| DirectError::new(ErrorCode::Internal, error.message))?;
        let deployment = DeploymentRef::new(self.function.clone(), number.to_string(), self.function.clone());
        let request = AgentRequest {
            method: "GET".to_owned(),
            path_and_query: "/readyz".to_owned(),
            headers: vec![("host".to_owned(), "graphrt-deploy.lambda".to_owned())],
            body: Vec::new(),
            request_id: format!("graphrt-deploy-check-{number}"),
        };
        let started = Instant::now();
        let mut last = String::from("no answer");
        let mut crashes = 0;
        while started.elapsed() < self.settings.health_timeout && crashes < MAX_START_CRASHES {
            match provider.invoke_agent(&deployment, &request).await {
                Ok(response) if response.head.status == 200 => return Ok(()),
                Ok(response) => {
                    crashes = 0;
                    last = format!("/readyz answered {}", response.head.status);
                }
                Err(error) => {
                    // The process exited or the adapter gave up: it will not get better by waiting.
                    if matches!(error, InvokeError::Function { .. }) {
                        crashes += 1;
                    }
                    last = error.to_string();
                }
            }
            tokio::time::sleep(self.settings.poll_interval).await;
        }
        let why = if crashes >= MAX_START_CRASHES {
            format!("it stopped {crashes} times while starting")
        } else {
            format!("it was not ready within {} s", self.settings.health_timeout.as_secs())
        };
        Err(DirectError::new(
            ErrorCode::HealthCheckFailed,
            format!(
                "the new version does not start: {why} ({last}). The previous version keeps serving. \
                 The function's logs are in CloudWatch, log group {} (AWS console, CloudWatch, Log groups)",
                self.log_group()
            ),
        ))
    }

    /// Creates the function or updates its configuration and code, then publishes a version.
    async fn publish(
        &self,
        configuration: &Value,
        image_uri: &str,
        architecture: &str,
        exists: bool,
        description: &str,
    ) -> Result<u64, DirectError> {
        if exists {
            self.wait_settled(false).await?;
            self.lambda
                .json_rest("PUT", &function_path(&self.function, "/configuration"), &[], Some(configuration))
                .await
                .map_err(|error| DirectError::from_aws(error, "update the function"))?;
            self.wait_ready().await?;
            let code = json!({ "ImageUri": image_uri, "Architectures": [architecture] });
            self.lambda
                .json_rest("PUT", &function_path(&self.function, "/code"), &[], Some(&code))
                .await
                .map_err(|error| DirectError::from_aws(error, "update the function's image"))?;
        } else {
            let mut body = configuration.clone();
            if let Some(object) = body.as_object_mut() {
                object.insert("FunctionName".into(), json!(self.function));
                object.insert("PackageType".into(), json!("Image"));
                object.insert("Code".into(), json!({ "ImageUri": image_uri }));
                object.insert("Architectures".into(), json!([architecture]));
                object.insert("Publish".into(), json!(false));
                object.insert(
                    "Tags".into(),
                    json!({ "graphrt:deployment": self.function, "graphrt:managed-by": "graphrt deploy --direct" }),
                );
            }
            let started = Instant::now();
            loop {
                match self.lambda.json_rest("POST", "/2015-03-31/functions", &[], Some(&body)).await {
                    Ok(_) => break,
                    Err(error) if is_conflict(&error) => break,
                    // A role made seconds ago is not usable by Lambda yet.
                    Err(error)
                        if error.message.contains("cannot be assumed by Lambda")
                            && started.elapsed() < self.settings.ready_timeout =>
                    {
                        tokio::time::sleep(self.settings.poll_interval * 2).await;
                    }
                    Err(error) => return Err(DirectError::from_aws(error, "create the Lambda function")),
                }
            }
        }
        self.wait_ready().await?;
        let published = self
            .lambda
            .json_rest(
                "POST",
                &function_path(&self.function, "/versions"),
                &[],
                Some(&json!({ "Description": description })),
            )
            .await
            .map_err(|error| DirectError::from_aws(error, "publish the version"))?;
        published
            .get("Version")
            .and_then(Value::as_str)
            .and_then(|version| version.parse().ok())
            .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "Lambda published no version number"))
    }

    fn configuration(
        &self,
        role_arn: &str,
        environment: &BTreeMap<String, String>,
        command: &[String],
        memory: u64,
        description: &str,
    ) -> Value {
        json!({
            "Role": role_arn,
            "Timeout": FUNCTION_TIMEOUT_SECS,
            "MemorySize": memory,
            "Environment": { "Variables": environment },
            "Description": description,
            "ImageConfig": { "EntryPoint": ["graphrt"], "Command": command, "WorkingDirectory": WORKFLOW_DIR },
            "LoggingConfig": { "LogFormat": "Text", "LogGroup": self.log_group() },
        })
    }

    /// Deletes versions beyond the newest `keep_versions` labels (never `live`) and their images.
    async fn prune(&self, live: u64) -> Result<Vec<String>, DirectError> {
        let versions = self.versions().await?;
        let mut kept_labels: Vec<String> = Vec::new();
        for version in &versions {
            if !kept_labels.contains(&version.label) && kept_labels.len() < self.settings.keep_versions {
                kept_labels.push(version.label.clone());
            }
        }
        if let Some(current) = versions.iter().find(|version| version.number == live)
            && !kept_labels.contains(&current.label)
        {
            kept_labels.push(current.label.clone());
        }
        let mut removed_labels = Vec::new();
        for version in versions.iter().filter(|version| version.number != live) {
            let newest_of_label = versions.iter().find(|other| other.label == version.label).map(|other| other.number);
            if kept_labels.contains(&version.label) && newest_of_label == Some(version.number) {
                continue;
            }
            let number = version.number.to_string();
            match self
                .lambda
                .json_rest("DELETE", &function_path(&self.function, ""), &[("Qualifier", number.as_str())], None)
                .await
            {
                Ok(_) => {}
                Err(error) if error.kind == ProviderErrorKind::NotFound => {}
                Err(error) => return Err(DirectError::from_aws(error, "delete an old version")),
            }
            if !kept_labels.contains(&version.label) && !removed_labels.contains(&version.label) {
                removed_labels.push(version.label.clone());
            }
        }
        Ecr { client: self.ecr.clone() }.delete_tags(&self.repository(), &removed_labels).await?;
        Ok(removed_labels)
    }

    /// Deploys `request` as a new version and makes it serve.
    ///
    /// @throws DeployFailure - with the new API key when it was already stored
    pub async fn deploy(
        &self,
        request: &DeployRequest,
        progress: Progress<'_>,
    ) -> Result<DeployOutcome, DeployFailure> {
        // The step still running when an error stops the deploy gets its `failed` event here.
        let mut open: Option<Step> = None;
        let result = {
            let mut tracking = |event: StepEvent| {
                open = (event.status == StepStatus::Running).then_some(event.step);
                progress(event);
            };
            self.deploy_steps(request, &mut tracking).await
        };
        if let (Err(failure), Some(step)) = (&result, open) {
            progress(StepEvent { step, status: StepStatus::Failed, message: failure.error.message.clone() });
        }
        result
    }

    async fn deploy_steps(
        &self,
        request: &DeployRequest,
        progress: Progress<'_>,
    ) -> Result<DeployOutcome, DeployFailure> {
        let mut progress = progress;
        validate(request)?;
        event(&mut progress, Step::Preparing, StepStatus::Running, "checking the AWS credentials and the deployment");
        let account = account::account_id(&self.sts).await?;
        let existing = self.get_function(None).await?;
        let versions = if existing.is_some() { self.versions().await? } else { Vec::new() };
        if versions.iter().any(|version| version.label == request.version) {
            return Err(DirectError::invalid(format!(
                "version {} is already deployed: use a new --version, or `graphrt deploy rollback {}` to serve it again",
                request.version, request.version
            ))
            .into());
        }
        let existing_configuration = existing.as_ref().and_then(|function| function.get("Configuration")).cloned();
        let role_arn = match (&request.role_arn, &existing_configuration) {
            (Some(role), _) => role.clone(),
            (None, Some(configuration)) => match configuration.get("Role").and_then(Value::as_str) {
                Some(role) => role.to_owned(),
                None => account::ensure_role(&self.iam, &self.role_name(), &request.deployment).await?.0,
            },
            (None, None) => account::ensure_role(&self.iam, &self.role_name(), &request.deployment).await?.0,
        };
        if existing.is_none() {
            account::ensure_log_group(&self.logs, &self.log_group(), &request.deployment).await?;
        }
        event(
            &mut progress,
            Step::Preparing,
            StepStatus::Running,
            format!(
                "copying {} into ECR of account {account} with the agent (function {} in {})",
                request.image, self.function, self.settings.region
            ),
        );
        let (image_uri, architecture) = self.push_image(request, &account).await?;
        event(&mut progress, Step::Preparing, StepStatus::Done, format!("image {image_uri} ({architecture})"));

        event(&mut progress, Step::Secrets, StepStatus::Running, "API key and secrets");
        let previous = existing_configuration.as_ref().map(environment_of).unwrap_or_default();
        let mut secrets = secrets_of(&previous);
        secrets.extend(request.secrets.clone());
        let new_key = if request.rotate_api_key || !secrets.contains_key(API_KEY_ENV) {
            secrets.insert(API_KEY_ENV.to_owned(), request.api_key_candidate.clone());
            Some(request.api_key_candidate.clone())
        } else {
            None
        };
        let environment = full_environment(&secrets, &request.deployment, &request.version);
        if environment.iter().map(|(key, value)| key.len() + value.len()).sum::<usize>() > MAX_ENVIRONMENT_BYTES {
            return Err(DeployFailure {
                error: DirectError::invalid(
                    "the secrets take more than Lambda's 4 KB of environment: pass fewer or shorter --secret values",
                ),
                api_key: None,
            });
        }
        let names: Vec<&str> = secrets.keys().filter(|key| *key != API_KEY_ENV).map(String::as_str).collect();
        event(
            &mut progress,
            Step::Secrets,
            StepStatus::Done,
            format!(
                "{} API key, secrets: {}",
                if new_key.is_some() { "new" } else { "kept the" },
                if names.is_empty() { "none".to_owned() } else { names.join(", ") }
            ),
        );
        let failure = |error: DirectError| DeployFailure { error, api_key: new_key.clone() };

        event(&mut progress, Step::Starting, StepStatus::Running, "updating the function and publishing the version");
        let description = version_description(&request.version, &request.workflow, request.now_ms);
        let command = serve_command(&request.serve_flags);
        let configuration = self.configuration(
            &role_arn,
            &environment,
            &command,
            memory_mib(request.agent.unpacked_bytes),
            &description,
        );
        let number = self
            .publish(&configuration, &image_uri, &architecture, existing.is_some(), &description)
            .await
            .map_err(&failure)?;
        event(&mut progress, Step::Starting, StepStatus::Done, format!("Lambda version {number}"));

        event(
            &mut progress,
            Step::Checking,
            StepStatus::Running,
            "starting the new version and calling /readyz (first start takes a few seconds)",
        );
        let paused = self.reserved_concurrency().await.map_err(&failure)? == Some(0);
        if paused {
            self.lift_pause().await.map_err(&failure)?;
        }
        if let Err(error) = self.check_health(number).await {
            if paused {
                let _still_paused = self.set_pause().await;
            }
            // A version that never started is not kept for rollback.
            self.discard_version(number, &request.version).await;
            return Err(failure(error));
        }
        self.point_live(number).await.map_err(&failure)?;
        let url = self.ensure_function_url().await.map_err(&failure)?;
        let removed = self.prune(number).await.map_err(&failure)?;
        let mut message = format!("{} serves at {url}", request.version);
        if !removed.is_empty() {
            message.push_str(&format!(" (removed old version(s) {})", removed.join(", ")));
        }
        event(&mut progress, Step::Checking, StepStatus::Done, message);
        Ok(DeployOutcome {
            app: self.function.clone(),
            region: self.settings.region.clone(),
            url,
            version: request.version.clone(),
            workflow: request.workflow.clone(),
            api_key: new_key,
            serve: request.serve_summary.clone(),
            store: None,
            resumed: paused,
        })
    }

    /// Deletes Lambda version `number` and the image tag `label` (best effort:
    /// the deploy already failed, and a leftover is pruned by a later deploy).
    async fn discard_version(&self, number: u64, label: &str) {
        let qualifier = number.to_string();
        let path = function_path(&self.function, "");
        if let Err(error) = self.lambda.json_rest("DELETE", &path, &[("Qualifier", qualifier.as_str())], None).await {
            tracing::warn!(function = %self.function, version = number, error = %error.message, "could not delete a failed version");
        }
        let ecr = Ecr { client: self.ecr.clone() };
        if let Err(error) = ecr.delete_tags(&self.repository(), &[label.to_owned()]).await {
            tracing::warn!(function = %self.function, label, error = %error.message, "could not delete a failed version's image");
        }
    }

    async fn set_pause(&self) -> Result<(), DirectError> {
        let path = format!("/2017-10-31/functions/{}/concurrency", uri_encode(&self.function, false));
        self.lambda
            .json_rest("PUT", &path, &[], Some(&json!({ "ReservedConcurrentExecutions": 0 })))
            .await
            .map(|_| ())
            .map_err(|error| DirectError::from_aws(error, "pause the function"))
    }

    async fn lift_pause(&self) -> Result<(), DirectError> {
        let path = format!("/2017-10-31/functions/{}/concurrency", uri_encode(&self.function, false));
        match self.lambda.json_rest("DELETE", &path, &[], None).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(()),
            Err(error) => Err(DirectError::from_aws(error, "resume the function")),
        }
    }

    /// The deployment's state.
    pub async fn status(&self) -> Result<DeploymentStatus, DirectError> {
        let function = self.require_function().await?;
        let configuration = function.get("Configuration").cloned().unwrap_or(Value::Null);
        let versions = self.versions().await?;
        let live = self.live_version().await?;
        let current = live.and_then(|number| versions.iter().find(|version| version.number == number));
        let paused = function
            .get("Concurrency")
            .and_then(|concurrency| concurrency.get("ReservedConcurrentExecutions"))
            .and_then(Value::as_i64)
            == Some(0);
        let state = configuration.get("State").and_then(Value::as_str).unwrap_or("Active");
        let update = configuration.get("LastUpdateStatus").and_then(Value::as_str).unwrap_or("Successful");
        let status = if paused {
            DeploymentState::Paused
        } else if state == "Failed" {
            DeploymentState::Failed
        } else if state == "Pending" || update == "InProgress" {
            DeploymentState::Deploying
        } else if current.is_some() {
            DeploymentState::Online
        } else {
            DeploymentState::Offline
        };
        Ok(DeploymentStatus {
            app: self.function.clone(),
            region: self.settings.region.clone(),
            url: self.function_url().await?.unwrap_or_default(),
            status,
            current_version: current.map(|version| version.label.clone()),
            workflow: current.map(|version| version.workflow.clone()),
            mode: Some("scale-to-zero".to_owned()),
            last_deployed_at: current.map(|version| rfc3339(version.deployed_ms)),
            machines: Vec::new(),
            store: None,
        })
    }

    /// The versions kept for rollback, newest first (one entry per label).
    pub async fn list_versions(&self) -> Result<Vec<VersionSummary>, DirectError> {
        self.require_function().await?;
        let versions = self.versions().await?;
        let live = self.live_version().await?;
        let mut seen: Vec<&str> = Vec::new();
        let mut summaries = Vec::new();
        for version in &versions {
            if seen.contains(&version.label.as_str()) {
                continue;
            }
            seen.push(&version.label);
            let current = live.is_some_and(|number| {
                versions.iter().find(|v| v.label == version.label && v.number == number).is_some()
            });
            summaries.push(VersionSummary {
                label: version.label.clone(),
                deployed_at: Some(rfc3339(version.deployed_ms)),
                current,
                role: if current { "current" } else { "standby" }.to_owned(),
                mode: Some("scale-to-zero".to_owned()),
                workflow: Some(version.workflow.clone()),
                state: "active".to_owned(),
                lambda_version: version.number.to_string(),
            });
        }
        Ok(summaries)
    }

    /// Serves `label` again. When the secrets or the API key changed since that
    /// version, it is published again with today's secrets (and health checked),
    /// otherwise `live` just moves back to it.
    pub async fn rollback(&self, label: &str, progress: Progress<'_>) -> Result<ServingVersion, DirectError> {
        let mut progress = progress;
        let function = self.require_function().await?;
        let versions = self.versions().await?;
        let target = versions.iter().find(|version| version.label == label).cloned().ok_or_else(|| {
            DirectError::new(
                ErrorCode::NotFound,
                format!(
                    "version {label} is not kept (graphrt deploy list-versions shows the ones you can roll back to)"
                ),
            )
        })?;
        let configuration = function.get("Configuration").cloned().unwrap_or(Value::Null);
        let today = secrets_of(&environment_of(&configuration));
        let number = if secrets_of(&target.environment) == today {
            event(&mut progress, Step::Checking, StepStatus::Running, format!("moving live to {label}"));
            target.number
        } else {
            event(
                &mut progress,
                Step::Starting,
                StepStatus::Running,
                format!("publishing {label} again with the current API key and secrets"),
            );
            let old = self.get_function(Some(&target.number.to_string())).await?.ok_or_else(|| {
                DirectError::new(ErrorCode::NotFound, format!("Lambda version {} is gone", target.number))
            })?;
            let old_configuration = old.get("Configuration").cloned().unwrap_or(Value::Null);
            let image_uri = old
                .get("Code")
                .and_then(|code| code.get("ImageUri"))
                .and_then(Value::as_str)
                .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "the old version has no image"))?
                .to_owned();
            let architecture = old_configuration
                .get("Architectures")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_str)
                .unwrap_or("x86_64")
                .to_owned();
            let command: Vec<String> = old_configuration
                .get("ImageConfigResponse")
                .and_then(|response| response.get("ImageConfig"))
                .and_then(|config| config.get("Command"))
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_else(|| serve_command(&[]));
            let deployment =
                target.environment.get("GRAPHRT_DEPLOYMENT").cloned().unwrap_or_else(|| self.function.clone());
            let environment = full_environment(&today, &deployment, label);
            let memory = old_configuration.get("MemorySize").and_then(Value::as_u64).unwrap_or(512);
            let role = configuration.get("Role").and_then(Value::as_str).unwrap_or_default().to_owned();
            let description = version_description(label, &target.workflow, target.deployed_ms);
            let new_configuration = self.configuration(&role, &environment, &command, memory, &description);
            let number = self.publish(&new_configuration, &image_uri, &architecture, true, &description).await?;
            event(&mut progress, Step::Starting, StepStatus::Done, format!("Lambda version {number}"));
            event(&mut progress, Step::Checking, StepStatus::Running, "calling /readyz");
            self.check_health(number).await?;
            number
        };
        self.point_live(number).await?;
        let url = self.ensure_function_url().await?;
        event(&mut progress, Step::Checking, StepStatus::Done, format!("{label} serves at {url}"));
        Ok(ServingVersion {
            app: self.function.clone(),
            region: self.settings.region.clone(),
            url,
            version: label.to_owned(),
            workflow: Some(target.workflow),
        })
    }

    /// Refuses every request (reserved concurrency 0). `false` when already paused.
    pub async fn pause(&self) -> Result<bool, DirectError> {
        self.require_function().await?;
        if self.reserved_concurrency().await? == Some(0) {
            return Ok(false);
        }
        self.set_pause().await?;
        Ok(true)
    }

    /// Serves again after a pause; `None` when it was not paused.
    pub async fn resume(&self, progress: Progress<'_>) -> Result<Option<ServingVersion>, DirectError> {
        self.require_function().await?;
        if self.reserved_concurrency().await? != Some(0) {
            return Ok(None);
        }
        let mut progress = progress;
        event(&mut progress, Step::Checking, StepStatus::Running, "lifting the pause");
        self.lift_pause().await?;
        let status = self.status().await?;
        event(&mut progress, Step::Checking, StepStatus::Done, format!("online at {}", status.url));
        Ok(Some(ServingVersion {
            app: self.function.clone(),
            region: self.settings.region.clone(),
            url: status.url,
            version: status.current_version.unwrap_or_default(),
            workflow: status.workflow,
        }))
    }

    /// Deletes the function, its image repository, its role (when graphrt made
    /// it) and its log group. `false` when there was no function.
    pub async fn remove(&self) -> Result<bool, DirectError> {
        let function = self.get_function(None).await?;
        let role_from_function = function
            .as_ref()
            .and_then(|function| function.get("Configuration"))
            .and_then(|configuration| configuration.get("Role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if function.is_some() {
            match self.lambda.json_rest("DELETE", &function_path(&self.function, ""), &[], None).await {
                Ok(_) => {}
                Err(error) if error.kind == ProviderErrorKind::NotFound => {}
                Err(error) => return Err(DirectError::from_aws(error, "delete the Lambda function")),
            }
        }
        Ecr { client: self.ecr.clone() }.delete_repository(&self.repository()).await?;
        let own_role = self.role_name();
        if role_from_function.as_deref().is_none_or(|arn| arn.ends_with(&format!("/{own_role}"))) {
            account::delete_role(&self.iam, &own_role).await?;
        }
        account::delete_log_group(&self.logs, &self.log_group()).await?;
        Ok(function.is_some())
    }
}

/// `serve /workflows --addr 0.0.0.0:8080` and the feature flags.
fn serve_command(flags: &[String]) -> Vec<String> {
    let mut command =
        vec!["serve".to_owned(), WORKFLOW_DIR.to_owned(), "--addr".to_owned(), format!("0.0.0.0:{AGENT_PORT}")];
    command.extend(flags.iter().cloned());
    command
}

fn full_environment(secrets: &BTreeMap<String, String>, deployment: &str, version: &str) -> BTreeMap<String, String> {
    let mut environment = secrets.clone();
    for (key, value) in [
        ("GRAPHRT_DEPLOYMENT", deployment.to_owned()),
        ("GRAPHRT_DEPLOYMENT_VERSION", version.to_owned()),
        ("AWS_LWA_PORT", AGENT_PORT.to_string()),
        ("AWS_LWA_INVOKE_MODE", "response_stream".to_owned()),
        ("AWS_LWA_READINESS_CHECK_PATH", "/readyz".to_owned()),
        // Lambda's file system is read-only except /tmp.
        ("HOME", "/tmp".to_owned()),
        ("TMPDIR", "/tmp".to_owned()),
    ] {
        environment.insert(key.to_owned(), value);
    }
    environment
}

/// `<workflow>.<extension of the agent file>`.
fn agent_file_name(request: &DeployRequest) -> String {
    let extension =
        request.agent.file_name.rsplit_once('.').map_or("yaml", |(_, extension)| extension).to_ascii_lowercase();
    format!("{}.{extension}", request.workflow)
}

fn validate(request: &DeployRequest) -> Result<(), DirectError> {
    if !is_version_label(&request.version) {
        return Err(DirectError::invalid(format!(
            "--version {:?} must be 1-64 letters, digits, dots, dashes or underscores, starting with a letter or digit (it is also the image tag)",
            request.version
        )));
    }
    if request.agent.contents.is_empty() {
        return Err(DirectError::invalid(format!("{} is empty", request.agent.file_name)));
    }
    if request.workflow.is_empty() || request.workflow.len() > 64 {
        return Err(DirectError::invalid("the agent file name must give a workflow name of 1-64 characters"));
    }
    if let Some(bad) = request.secrets.keys().find(|name| !crate::deploy::is_env_name(name)) {
        return Err(DirectError::invalid(format!("secret name {bad:?} is not a valid environment variable name")));
    }
    if let Some(reserved) = request
        .secrets
        .keys()
        .find(|name| name.as_str() == API_KEY_ENV || MANAGED_ENV.contains(&name.as_str()) || name.starts_with("AWS_"))
    {
        let hint = if reserved == API_KEY_ENV { " (use --rotate-api-key)" } else { "" };
        return Err(DirectError::invalid(format!(
            "{reserved} is managed by graphrt deploy or reserved by Lambda{hint}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_round_trip() {
        let text = version_description("v1.2", "support", 1_764_288_000_000);
        assert_eq!(parse_description(&text), Some(("v1.2".to_owned(), "support".to_owned(), 1_764_288_000_000)));
        assert_eq!(parse_description("made by hand"), None);
    }

    #[test]
    fn labels_are_image_tags() {
        assert!(is_version_label("v1") && is_version_label("2026-11-28.a_1"));
        assert!(
            !is_version_label("")
                && !is_version_label("-v1")
                && !is_version_label("v 1")
                && !is_version_label(&"a".repeat(65))
        );
    }

    #[test]
    fn dates_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_764_288_000_123), "2025-11-28T00:00:00.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn only_secrets_and_the_key_carry_over() {
        let environment = full_environment(
            &BTreeMap::from([
                ("OPENAI_API_KEY".to_owned(), "sk".to_owned()),
                (API_KEY_ENV.to_owned(), "grk".to_owned()),
            ]),
            "support",
            "v1",
        );
        assert_eq!(environment["AWS_LWA_INVOKE_MODE"], "response_stream");
        assert_eq!(environment["HOME"], "/tmp");
        let carried = secrets_of(&environment);
        assert_eq!(carried.keys().collect::<Vec<_>>(), [API_KEY_ENV, "OPENAI_API_KEY"]);
    }

    #[test]
    fn the_command_serves_the_workflow_dir_behind_the_adapter() {
        assert_eq!(
            serve_command(&["--chat".to_owned(), "support".to_owned()]),
            ["serve", "/workflows", "--addr", "0.0.0.0:8080", "--chat", "support"]
        );
    }

    fn request() -> DeployRequest {
        DeployRequest {
            deployment: "support".into(),
            version: "v1".into(),
            image: "ghcr.io/graphrt/graphrt:0.6.3".into(),
            adapter_image: super::super::DEFAULT_ADAPTER_IMAGE.into(),
            secrets: BTreeMap::new(),
            api_key_candidate: "grk_x".into(),
            rotate_api_key: false,
            agent: super::super::AgentFile {
                file_name: "support.yaml".into(),
                contents: b"x".to_vec(),
                unpacked_bytes: 1,
            },
            workflow: "support".into(),
            serve_flags: Vec::new(),
            serve_summary: Value::Null,
            role_arn: None,
            now_ms: 0,
        }
    }

    #[test]
    fn requests_are_checked_before_any_call() {
        assert!(validate(&request()).is_ok());
        assert_eq!(agent_file_name(&request()), "support.yaml");
        let mut reserved = request();
        reserved.secrets.insert(API_KEY_ENV.into(), "x".into());
        assert!(validate(&reserved).unwrap_err().message.contains("--rotate-api-key"));
        let mut aws = request();
        aws.secrets.insert("AWS_SECRET_ACCESS_KEY".into(), "x".into());
        assert!(validate(&aws).is_err());
        let mut label = request();
        label.version = "v 1".into();
        assert!(validate(&label).unwrap_err().message.contains("--version"));
    }

    #[test]
    fn memory_grows_with_the_unpacked_agent() {
        assert_eq!(memory_mib(1000), 512);
        assert_eq!(memory_mib(100 * 1024 * 1024), 2048);
        assert_eq!(memory_mib(1024 * 1024 * 1024), 3008);
    }
}
