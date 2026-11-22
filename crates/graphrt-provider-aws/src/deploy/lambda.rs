//! `deploy-lambda`: creates or updates a container-image function, publishes an
//! immutable version, points an alias named after the deployment version at it,
//! and (optionally) adds an IAM-authenticated Function URL with `RESPONSE_STREAM`.
//!
//! The returned `DeploymentRef` is `{name, version = alias, target = function ARN}`,
//! which [`crate::lambda::LambdaProvider`] invokes as `function:alias`.
//!
//! Provider settings (`DeploymentSpec::provider_settings`):
//!
//! | Key | Required | Meaning |
//! | --- | --- | --- |
//! | `role_arn` | yes | execution role of the function |
//! | `function_name` | no | default `graphrt-<name>` |
//! | `triggers` | no | run triggers, default `["sync"]`; anything else is rejected |
//! | `max_run_secs` | no | gateway cap, default and maximum 840 |
//! | `function_timeout_secs` | no | default `max_run_secs + 30` (≤ 900) |
//! | `container_port` | no | agent port behind Lambda Web Adapter (default 8080) |
//! | `architecture` | no | `x86_64` (default) or `arm64` |
//! | `function_url` | no | create the IAM Function URL (default `true`) |
//! | `secrets_mode` | no | `reject` (default) or `resolve-to-env` |
//! | `kms_key_arn` | no | customer key encrypting the environment |

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Settings, is_env_name, validate_secret_refs};
use crate::api::{
    BoxFuture, DeployDriver, DeployError, DeployErrorKind, DeploymentRef, DeploymentSpec, ProviderError,
    ProviderErrorKind, SecretRef,
};
use crate::auth::CredentialChain;
use crate::auth::sigv4::uri_encode;
use crate::lambda::{DeploymentCheck, LAMBDA_MAX_TIMEOUT_SECS, MAX_RUN_SECS, RunTrigger, validate_deployment};
use crate::rest::{ApiLimits, AwsRestClient, ServiceEndpoint, is_conflict};

/// Lambda's limit on the total size of environment variables.
const MAX_ENVIRONMENT_BYTES: usize = 4 * 1024;
const DEFAULT_PORT: u64 = 8080;

/// Endpoints and timing of the Lambda deploy driver.
#[derive(Clone, Debug)]
pub struct LambdaDeployConfig {
    pub lambda: ServiceEndpoint,
    /// Used only with `secrets_mode = resolve-to-env`.
    pub secrets_manager: ServiceEndpoint,
    pub ssm: ServiceEndpoint,
    pub limits: ApiLimits,
    /// How long to wait for a create / update to finish.
    pub ready_timeout: Duration,
    pub poll_interval: Duration,
}

impl LambdaDeployConfig {
    /// Public endpoints of `region`, 5 min ready timeout, 2 s polling.
    pub fn new(region: &str) -> Self {
        Self {
            lambda: ServiceEndpoint::regional("lambda", region),
            secrets_manager: ServiceEndpoint::regional("secretsmanager", region),
            ssm: ServiceEndpoint::regional("ssm", region),
            limits: ApiLimits::default(),
            ready_timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(2),
        }
    }
}

/// Creates / updates Lambda functions (`graphrt deploy --target lambda`).
pub struct LambdaDeployDriver {
    lambda: AwsRestClient,
    secrets_manager: AwsRestClient,
    ssm: AwsRestClient,
    ready_timeout: Duration,
    poll_interval: Duration,
}

impl fmt::Debug for LambdaDeployDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LambdaDeployDriver").field("lambda", &self.lambda).finish_non_exhaustive()
    }
}

/// How secrets reach the function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretsMode {
    /// Lambda has no secret references: refuse specs with secrets.
    Reject,
    /// Read each referenced secret at deploy time (deploy credentials) and store it as
    /// an environment variable, encrypted at rest with the function's KMS key.
    ResolveToEnv,
}

/// Everything the driver derived from a spec before calling AWS.
#[derive(Clone)]
pub struct LambdaPlan {
    pub function_name: String,
    pub alias: String,
    pub image: String,
    pub role_arn: String,
    pub timeout_secs: u64,
    pub memory_mib: u32,
    pub architecture: String,
    pub command: Vec<String>,
    /// Plain environment plus the Lambda Web Adapter settings (secrets added later).
    pub environment: BTreeMap<String, String>,
    pub secrets: Vec<SecretRef>,
    pub secrets_mode: SecretsMode,
    pub kms_key_arn: Option<String>,
    pub function_url: bool,
    pub description: String,
}

impl fmt::Debug for LambdaPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LambdaPlan")
            .field("function_name", &self.function_name)
            .field("alias", &self.alias)
            .field("image", &self.image)
            .field("timeout_secs", &self.timeout_secs)
            .field("secrets", &self.secrets.iter().map(|s| &s.env).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A valid alias name: `[A-Za-z0-9_-]{1,128}`, not only digits (that would be a version number).
pub fn is_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !name.bytes().all(|b| b.is_ascii_digit())
}

fn parse_trigger(name: &str) -> Result<RunTrigger, DeployError> {
    match name {
        "sync" => Ok(RunTrigger::Sync),
        "async" => Ok(RunTrigger::Async),
        "webhook" => Ok(RunTrigger::Webhook),
        "cron" => Ok(RunTrigger::Cron),
        other => Err(DeployError::invalid(format!("unknown trigger {other:?}"))),
    }
}

fn is_ecr_image(image: &str) -> bool {
    let host = image.split('/').next().unwrap_or_default();
    host.contains(".dkr.ecr.") && host.contains(".amazonaws.com")
}

/// Validate `spec` and derive the function settings.
///
/// @throws `DeployError::invalid` when Lambda cannot run the spec.
pub fn plan(spec: &DeploymentSpec) -> Result<LambdaPlan, DeployError> {
    let settings = Settings(&spec.provider_settings);
    let image =
        spec.image.clone().filter(|i| !i.is_empty()).ok_or_else(|| DeployError::invalid("Lambda needs an image"))?;
    if !is_ecr_image(&image) {
        return Err(DeployError::invalid("Lambda container images must come from Amazon ECR"));
    }
    if !is_alias_name(&spec.version) {
        return Err(DeployError::invalid(format!(
            "version {:?} must be a Lambda alias name ([A-Za-z0-9_-], not only digits)",
            spec.version
        )));
    }
    let triggers = settings.strings("triggers")?;
    let triggers: Vec<RunTrigger> = if triggers.is_empty() {
        vec![RunTrigger::Sync]
    } else {
        triggers.into_iter().map(parse_trigger).collect::<Result<_, _>>()?
    };
    let max_run_secs = settings.u64("max_run_secs")?.unwrap_or(MAX_RUN_SECS);
    let timeout_secs =
        settings.u64("function_timeout_secs")?.unwrap_or_else(|| (max_run_secs + 30).min(LAMBDA_MAX_TIMEOUT_SECS));
    validate_deployment(&DeploymentCheck {
        triggers: &triggers,
        max_run_secs,
        function_timeout_secs: timeout_secs,
        lwa_invoke_mode: spec.env.get("AWS_LWA_INVOKE_MODE").map(String::as_str),
    })?;
    if let Some(key) =
        spec.env.keys().find(|k| !is_env_name(k) || (k.starts_with("AWS_") && !k.starts_with("AWS_LWA_")))
    {
        return Err(DeployError::invalid(format!("env name {key:?} is invalid or reserved by Lambda")));
    }
    let port = settings.u64("container_port")?.unwrap_or(DEFAULT_PORT);
    let mut environment = spec.env.clone();
    environment.insert("AWS_LWA_INVOKE_MODE".into(), "response_stream".into());
    environment.insert("AWS_LWA_PORT".into(), port.to_string());
    environment.entry("AWS_LWA_READINESS_CHECK_PATH".into()).or_insert_with(|| "/healthz".into());
    let memory_mib = if spec.resources.memory_mib == 0 { 1024 } else { spec.resources.memory_mib };
    if !(128..=10_240).contains(&memory_mib) {
        return Err(DeployError::invalid(format!("Lambda memory must be 128–10240 MiB (got {memory_mib})")));
    }
    let architecture = settings.string("architecture")?.unwrap_or("x86_64").to_owned();
    if !matches!(architecture.as_str(), "x86_64" | "arm64") {
        return Err(DeployError::invalid("architecture must be x86_64 or arm64"));
    }
    let secrets_mode = match settings.string("secrets_mode")?.unwrap_or("reject") {
        "reject" => SecretsMode::Reject,
        "resolve-to-env" => SecretsMode::ResolveToEnv,
        other => return Err(DeployError::invalid(format!("secrets_mode {other:?} must be reject or resolve-to-env"))),
    };
    validate_secret_refs(&spec.secrets)?;
    if !spec.secrets.is_empty() && secrets_mode == SecretsMode::Reject {
        return Err(DeployError {
            kind: DeployErrorKind::Unsupported,
            message: "Lambda has no secret references: set secrets_mode = resolve-to-env (values stored as \
                      KMS-encrypted environment variables at deploy time) or deploy to ECS Fargate"
                .into(),
        });
    }
    Ok(LambdaPlan {
        function_name: settings
            .string("function_name")?
            .map_or_else(|| format!("graphrt-{}", spec.name), str::to_owned),
        alias: spec.version.clone(),
        image,
        role_arn: settings.required("role_arn")?.to_owned(),
        timeout_secs,
        memory_mib,
        architecture,
        command: spec.command.clone(),
        environment,
        secrets: spec.secrets.clone(),
        secrets_mode,
        kms_key_arn: settings.string("kms_key_arn")?.map(str::to_owned),
        function_url: settings.bool("function_url")?.unwrap_or(true),
        description: format!("graphrt {} {} ({})", spec.name, spec.version, spec.workflow_fingerprint),
    })
}

/// Split a Secrets Manager reference into `(secret ARN, JSON key)` (ECS `valueFrom` syntax:
/// `arn:…:secret:name-AbCdEf[:json-key[:version-stage[:version-id]]]`).
fn split_secret_reference(reference: &str) -> (&str, Option<&str>) {
    // arn : partition : service : region : account : "secret" : name [: key ...]
    let mut colon_positions = reference.match_indices(':').map(|(i, _)| i);
    match colon_positions.nth(6) {
        Some(at) => {
            let key = reference.get(at + 1..).and_then(|rest| rest.split(':').next()).filter(|k| !k.is_empty());
            (reference.get(..at).unwrap_or(reference), key)
        }
        None => (reference, None),
    }
}

fn environment_size(environment: &BTreeMap<String, String>) -> usize {
    environment.iter().map(|(k, v)| k.len() + v.len()).sum()
}

fn function_path(function: &str, suffix: &str) -> String {
    format!("/2015-03-31/functions/{}{suffix}", uri_encode(function, false))
}

impl LambdaDeployDriver {
    /// A driver signing with deploy-time credentials from `chain`.
    ///
    /// @throws `ProviderError::fatal` when an HTTP client cannot be built.
    pub fn new(chain: Arc<CredentialChain>, config: LambdaDeployConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            lambda: AwsRestClient::new(Arc::clone(&chain), config.lambda, config.limits)?,
            secrets_manager: AwsRestClient::new(Arc::clone(&chain), config.secrets_manager, config.limits)?,
            ssm: AwsRestClient::new(chain, config.ssm, config.limits)?,
            ready_timeout: config.ready_timeout,
            poll_interval: config.poll_interval,
        })
    }

    /// Read one secret value with the deploy credentials (never logged).
    async fn resolve_secret(&self, secret: &SecretRef) -> Result<String, DeployError> {
        let not_text = || DeployError::invalid(format!("secret {} has no string value", secret.env));
        if secret.reference.contains(":ssm:") {
            let body = json!({ "Name": secret.reference, "WithDecryption": true });
            let response = self.ssm.json_11("AmazonSSM.GetParameter", &body).await?;
            let value = response.get("Parameter").and_then(|p| p.get("Value")).and_then(Value::as_str);
            return value.map(str::to_owned).ok_or_else(not_text);
        }
        let (arn, key) = split_secret_reference(&secret.reference);
        let response =
            self.secrets_manager.json_11("secretsmanager.GetSecretValue", &json!({ "SecretId": arn })).await?;
        let text = response.get("SecretString").and_then(Value::as_str).ok_or_else(not_text)?;
        let Some(key) = key else { return Ok(text.to_owned()) };
        let document: Value = serde_json::from_str(text)
            .map_err(|_| DeployError::invalid(format!("secret {} is not a JSON document", secret.env)))?;
        document
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| DeployError::invalid(format!("secret {} has no key {key}", secret.env)))
    }

    async fn environment(&self, plan: &LambdaPlan) -> Result<BTreeMap<String, String>, DeployError> {
        let mut environment = plan.environment.clone();
        for secret in &plan.secrets {
            let value = self.resolve_secret(secret).await?;
            environment.insert(secret.env.clone(), value);
        }
        if environment_size(&environment) > MAX_ENVIRONMENT_BYTES {
            return Err(DeployError::invalid("Lambda environment exceeds 4 KB (keys + values)"));
        }
        Ok(environment)
    }

    fn configuration(plan: &LambdaPlan, environment: &BTreeMap<String, String>) -> Value {
        let mut body = json!({
            "Role": plan.role_arn,
            "Timeout": plan.timeout_secs,
            "MemorySize": plan.memory_mib,
            "Environment": { "Variables": environment },
            "Description": plan.description,
        });
        if let Some(object) = body.as_object_mut() {
            if !plan.command.is_empty() {
                object.insert("ImageConfig".into(), json!({ "Command": plan.command }));
            }
            if let Some(key) = &plan.kms_key_arn {
                object.insert("KMSKeyArn".into(), json!(key));
            }
        }
        body
    }

    /// Wait until the function is Active and its last update finished.
    async fn wait_ready(&self, function: &str) -> Result<Value, DeployError> {
        let started = Instant::now();
        loop {
            let configuration =
                self.lambda.json_rest("GET", &function_path(function, "/configuration"), &[], None).await?;
            let state = configuration.get("State").and_then(Value::as_str).unwrap_or("Active");
            let update = configuration.get("LastUpdateStatus").and_then(Value::as_str).unwrap_or("Successful");
            if state == "Failed" || update == "Failed" {
                let reason = configuration
                    .get("LastUpdateStatusReason")
                    .or_else(|| configuration.get("StateReason"))
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(DeployError::from(ProviderError::fatal(format!("function {function} failed: {reason}"))));
            }
            if state == "Active" && update == "Successful" {
                return Ok(configuration);
            }
            if started.elapsed() >= self.ready_timeout {
                return Err(DeployError::from(ProviderError::transient(format!(
                    "function {function} not ready after {} s (State {state}, LastUpdateStatus {update})",
                    self.ready_timeout.as_secs()
                ))));
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Create the function, or update its configuration and code.
    async fn upsert_function(
        &self,
        plan: &LambdaPlan,
        environment: &BTreeMap<String, String>,
    ) -> Result<(), DeployError> {
        let existing = self.lambda.json_rest("GET", &function_path(&plan.function_name, ""), &[], None).await;
        match existing {
            Err(error) if error.kind == ProviderErrorKind::NotFound => {
                let mut body = Self::configuration(plan, environment);
                if let Some(object) = body.as_object_mut() {
                    object.insert("FunctionName".into(), json!(plan.function_name));
                    object.insert("PackageType".into(), json!("Image"));
                    object.insert("Code".into(), json!({ "ImageUri": plan.image }));
                    object.insert("Architectures".into(), json!([plan.architecture]));
                    object.insert("Publish".into(), json!(false));
                    object.insert("Tags".into(), json!({ "graphrt:deployment": plan.function_name }));
                }
                match self.lambda.json_rest("POST", "/2015-03-31/functions", &[], Some(&body)).await {
                    Ok(_) => {
                        tracing::info!(function = %plan.function_name, "lambda function created");
                        return Ok(());
                    }
                    // Created concurrently (or a retried create): update it instead.
                    Err(error) if is_conflict(&error) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        self.wait_ready(&plan.function_name).await?;
        let configuration = Self::configuration(plan, environment);
        self.lambda
            .json_rest("PUT", &function_path(&plan.function_name, "/configuration"), &[], Some(&configuration))
            .await?;
        self.wait_ready(&plan.function_name).await?;
        let code = json!({ "ImageUri": plan.image, "Architectures": [plan.architecture] });
        self.lambda.json_rest("PUT", &function_path(&plan.function_name, "/code"), &[], Some(&code)).await?;
        tracing::info!(function = %plan.function_name, "lambda function updated");
        Ok(())
    }

    /// Point the alias at `version`. An alias that already points elsewhere is an
    /// error: deployment versions are immutable.
    async fn ensure_alias(&self, plan: &LambdaPlan, version: &str) -> Result<(), DeployError> {
        let alias_path = function_path(&plan.function_name, &format!("/aliases/{}", plan.alias));
        match self.lambda.json_rest("GET", &alias_path, &[], None).await {
            Ok(alias) => {
                let current = alias.get("FunctionVersion").and_then(Value::as_str).unwrap_or_default();
                if current == version {
                    return Ok(());
                }
                Err(DeployError::invalid(format!(
                    "version {} is already deployed as function version {current}; deploy a new version name",
                    plan.alias
                )))
            }
            Err(error) if error.kind == ProviderErrorKind::NotFound => {
                let body = json!({ "Name": plan.alias, "FunctionVersion": version, "Description": plan.description });
                self.lambda
                    .json_rest("POST", &function_path(&plan.function_name, "/aliases"), &[], Some(&body))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// IAM-authenticated Function URL on the alias, streaming responses.
    async fn ensure_function_url(&self, plan: &LambdaPlan) -> Result<Option<String>, DeployError> {
        let path = format!("/2021-10-31/functions/{}/url", uri_encode(&plan.function_name, false));
        let query = [("Qualifier", plan.alias.as_str())];
        let body = json!({ "AuthType": "AWS_IAM", "InvokeMode": "RESPONSE_STREAM" });
        let response = match self.lambda.json_rest("POST", &path, &query, Some(&body)).await {
            Ok(response) => response,
            Err(error) if is_conflict(&error) => self.lambda.json_rest("PUT", &path, &query, Some(&body)).await?,
            Err(error) => return Err(error.into()),
        };
        Ok(response.get("FunctionUrl").and_then(Value::as_str).map(str::to_owned))
    }

    async fn deploy(&self, spec: &DeploymentSpec) -> Result<DeploymentRef, DeployError> {
        let plan = plan(spec)?;
        let environment = self.environment(&plan).await?;
        self.upsert_function(&plan, &environment).await?;
        self.wait_ready(&plan.function_name).await?;
        let published = self
            .lambda
            .json_rest(
                "POST",
                &function_path(&plan.function_name, "/versions"),
                &[],
                Some(&json!({ "Description": plan.description })),
            )
            .await?;
        let version = published.get("Version").and_then(Value::as_str).unwrap_or_default().to_owned();
        let function_arn = published
            .get("FunctionArn")
            .and_then(Value::as_str)
            .map(|arn| arn.strip_suffix(&format!(":{version}")).unwrap_or(arn).to_owned())
            .ok_or_else(|| DeployError::from(ProviderError::transient("PublishVersion returned no FunctionArn")))?;
        self.ensure_alias(&plan, &version).await?;
        let url = if plan.function_url { self.ensure_function_url(&plan).await? } else { None };
        tracing::info!(function = %plan.function_name, alias = %plan.alias, version = %version, url = ?url, "lambda deployment ready");
        Ok(DeploymentRef::new(spec.name.clone(), plan.alias, function_arn))
    }
}

impl DeployDriver for LambdaDeployDriver {
    fn apply<'a>(&'a self, spec: &'a DeploymentSpec) -> BoxFuture<'a, Result<DeploymentRef, DeployError>> {
        Box::pin(self.deploy(spec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_names() {
        assert!(is_alias_name("v7") && is_alias_name("2026-11-22_a"));
        assert!(!is_alias_name("7") && !is_alias_name("") && !is_alias_name("v 7"));
    }

    #[test]
    fn secret_reference_split() {
        let arn = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:llm-AbCdEf";
        assert_eq!(split_secret_reference(arn), (arn, None));
        assert_eq!(split_secret_reference(&format!("{arn}:openai::")), (arn, Some("openai")));
    }

    #[test]
    fn ecr_images_only() {
        assert!(is_ecr_image("123456789012.dkr.ecr.eu-west-1.amazonaws.com/graphrt:0.5"));
        assert!(!is_ecr_image("ghcr.io/bimapangestu28/graphrt:0.5"));
    }
}
