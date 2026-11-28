//! The operations of [`CloudRunDirect`]: deploy, status, versions, rollback,
//! pause, resume and remove.

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_api::direct::{Step, StepEvent, StepStatus, workflow_name_of};
use serde_json::{Value, json};

use super::google::{Body, Method};
use super::naming::{secret_id_for, tag_for};
use super::resources::{AGENTS_PREFIX, DeploymentSecret, STORE_PREFIX};
use super::service::{
    AGENT_LABEL, MODE_LABEL, PAUSED_LABEL, RevisionPlan, STORE_LABEL, Target, VERSION_ANNOTATION, WORKFLOW_LABEL,
    candidate_traffic, check_ours, current_targets, failure_message, promoted_traffic, render_service, render_template,
    short_name, traffic_summary, without_tag,
};
use super::{
    API_KEY_ENV, CloudRunDirect, DeployOutcome, DeployRequest, DeploymentState, DeploymentStatus, DirectError,
    ErrorCode, ServingVersion, StoreSummary, VersionSummary, emit,
};

/// Receives progress events.
pub type Progress<'callback> = &'callback mut dyn FnMut(StepEvent);

/// A failed deploy. `api_key` is set when the deploy already stored a new API
/// key before failing: it is the only time the caller can learn it.
#[derive(Clone, PartialEq, Eq)]
pub struct DeployFailure {
    pub error: DirectError,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for DeployFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeployFailure")
            .field("error", &self.error)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl From<DirectError> for DeployFailure {
    fn from(error: DirectError) -> Self {
        Self { error, api_key: None }
    }
}

/// Role the service account gets on its bucket: read, write, list and delete objects.
const BUCKET_ROLE: &str = "roles/storage.objectUser";
/// Default image repository (the published graphrt image).
const IMAGE_REPOSITORY: &str = "ghcr.io/graphrt/graphrt";

/// What a deploy needs of the project before the revision.
struct Prepared {
    project_number: Option<String>,
    image: String,
    agent_dir: String,
    existing: Option<Value>,
}

impl CloudRunDirect {
    fn service_resource(&self) -> String {
        format!("projects/{}/locations/{}/services/{}", self.project(), self.region(), self.service)
    }

    fn service_url(&self) -> String {
        format!("{}/v2/{}", self.client.endpoints.run, self.service_resource())
    }

    async fn get_service(&self) -> Result<Option<Value>, DirectError> {
        self.client.get_optional(&self.service_url(), &format!("read the Cloud Run service {}", self.service)).await
    }

    async fn existing_direct_service(&self) -> Result<Value, DirectError> {
        let service = self.get_service().await?.ok_or_else(|| self.not_found())?;
        check_ours(&service, &self.service)?;
        Ok(service)
    }

    fn not_found(&self) -> DirectError {
        DirectError::new(
            ErrorCode::NotFound,
            format!(
                "there is no deployment {} in project {} region {} (deploy it first, or pass the --app, --project \
                 and --region it was deployed with)",
                self.service,
                self.project(),
                self.region()
            ),
        )
    }

    /// `PATCH` the service (`update_mask` empty: the whole service, created when
    /// missing) and waits for the rollout.
    async fn patch_service(&self, body: &Value, update_mask: &str, what: &str) -> Result<Value, DirectError> {
        let url = if update_mask.is_empty() {
            format!("{}?allowMissing=true", self.service_url())
        } else {
            format!("{}?updateMask={update_mask}", self.service_url())
        };
        let operation = self.client.send(Method::Patch, &url, Body::Json(body), what).await?;
        let run = self.client.endpoints.run.clone();
        let response = self
            .client
            .wait_operation(&run, "v2", operation, self.settings.start_timeout, self.settings.poll_interval, what)
            .await?;
        if response.is_object() && response.get("name").is_some() {
            return Ok(response);
        }
        self.get_service().await?.ok_or_else(|| self.not_found())
    }

    /// The URL of the service (`uri`), or the one the public check uses in tests.
    fn public_base(&self, service: &Value) -> String {
        match &self.settings.public_url_override {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => service.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned(),
        }
    }

    /// Deploys `request.version` and moves all traffic to it once it is ready.
    ///
    /// @param progress - receives every step event
    /// @returns The deployment's URL, version and (when created or rotated) API key
    /// @throws DeployFailure - with the API key when it was stored before the failure
    pub async fn deploy(
        &self,
        request: &DeployRequest,
        progress: Progress<'_>,
    ) -> Result<DeployOutcome, DeployFailure> {
        validate_request(request)?;
        let workflow = workflow_name_of(&request.agent.file_name);
        emit(
            progress,
            Step::Preparing,
            StepStatus::Running,
            format!("Preparing Google Cloud project {} ({})", self.project(), self.region()),
        );
        let prepared = match self.prepare(request, &workflow).await {
            Ok(prepared) => prepared,
            Err(error) => {
                emit(progress, Step::Preparing, StepStatus::Failed, error.message.clone());
                return Err(error.into());
            }
        };
        emit(
            progress,
            Step::Preparing,
            StepStatus::Done,
            format!(
                "bucket gs://{}, service account {}, image {}",
                self.bucket(),
                self.service_account_email(),
                prepared.image
            ),
        );
        emit(progress, Step::Secrets, StepStatus::Running, "Writing secrets to Secret Manager");
        let (secrets, new_key) = match self.write_secrets(request).await {
            Ok(written) => written,
            Err(error) => {
                emit(progress, Step::Secrets, StepStatus::Failed, error.message.clone());
                return Err(error.into());
            }
        };
        let names: Vec<&str> = secrets.iter().map(|secret| secret.env.as_str()).collect();
        emit(progress, Step::Secrets, StepStatus::Done, format!("set {}", names.join(", ")));
        let fail = |error: DirectError| DeployFailure { error, api_key: new_key.clone() };
        self.roll_out(request, &workflow, &prepared, &secrets, progress).await.map_err(fail).map(|(service, store)| {
            DeployOutcome {
                app: self.service.clone(),
                url: service.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned(),
                version: request.version.clone(),
                workflow,
                api_key: new_key.clone(),
                serve: request.serve.summary(),
                store,
                project: self.project().to_owned(),
                region: self.region().to_owned(),
                image: prepared.image.clone(),
            }
        })
    }

    async fn prepare(&self, request: &DeployRequest, workflow: &str) -> Result<Prepared, DirectError> {
        let project_number = self.ensure_apis().await?;
        let existing = self.get_service().await?;
        if let Some(service) = &existing {
            check_ours(service, &self.service)?;
        }
        let account = self.ensure_service_account(&request.deployment).await?;
        self.ensure_bucket(&request.deployment).await?;
        self.grant_bucket(&format!("serviceAccount:{account}"), BUCKET_ROLE).await?;
        let image = request.image.clone().unwrap_or_else(default_image);
        let image = self.resolve_image(&image).await?;
        let agent_dir = agent_dir_of(&request.agent.contents);
        let object = format!("{AGENTS_PREFIX}{agent_dir}/{}", agent_object_name(&request.agent.file_name, workflow));
        self.upload_object(&object, &request.agent.contents).await?;
        Ok(Prepared { project_number, image, agent_dir, existing })
    }

    /// Stores the API key (first deploy or rotation) and the `--secret` values.
    ///
    /// @returns Every secret of the deployment, and the new API key when one was stored
    async fn write_secrets(
        &self,
        request: &DeployRequest,
    ) -> Result<(Vec<DeploymentSecret>, Option<String>), DirectError> {
        let key_id = secret_id_for(&self.service, API_KEY_ENV);
        let mut new_key = None;
        if request.rotate_api_key || !self.secret_has_version(&key_id).await? {
            self.put_secret(&request.deployment, API_KEY_ENV, &request.api_key_candidate).await?;
            new_key = Some(request.api_key_candidate.clone());
        }
        for (name, value) in &request.secrets {
            self.put_secret(&request.deployment, name, value).await?;
        }
        let mut secrets = self.list_secrets().await?;
        // Secret Manager lists are eventually consistent: a secret created a
        // moment ago may be missing. Every secret of this deploy is known anyway.
        for env in std::iter::once(API_KEY_ENV).chain(request.secrets.keys().map(String::as_str)) {
            if !secrets.iter().any(|secret| secret.env == env) {
                let id = secret_id_for(&self.service, env);
                secrets.push(DeploymentSecret {
                    name: format!("projects/{}/secrets/{id}", self.project()),
                    id,
                    env: env.to_owned(),
                });
            }
        }
        secrets.sort_by(|a, b| a.env.cmp(&b.env));
        Ok((secrets, new_key))
    }

    /// Starts the revision without traffic, promotes it once ready, checks it from outside.
    async fn roll_out(
        &self,
        request: &DeployRequest,
        workflow: &str,
        prepared: &Prepared,
        secrets: &[DeploymentSecret],
        progress: Progress<'_>,
    ) -> Result<(Value, Option<StoreSummary>), DirectError> {
        let tag = tag_for(&request.version);
        let existing_targets = prepared.existing.as_ref().map(current_targets).unwrap_or_default();
        // Pairing links need the address before the first deploy has one: Cloud
        // Run's deterministic URL, `https://<service>-<project number>.<region>.run.app`.
        let public_url = prepared
            .existing
            .as_ref()
            .and_then(|service| service.get("uri").and_then(Value::as_str))
            .map(str::to_owned)
            .or_else(|| {
                prepared
                    .project_number
                    .as_ref()
                    .map(|number| format!("https://{}-{number}.{}.run.app", self.service, self.region()))
            })
            .unwrap_or_default();
        emit(progress, Step::Starting, StepStatus::Running, format!("Starting version {}", request.version));
        let labels = self.labels(&request.deployment);
        let plan = RevisionPlan {
            request,
            workflow,
            image: &prepared.image,
            bucket: &self.bucket(),
            agent_dir: &prepared.agent_dir,
            service_account: &self.service_account_email(),
            secrets,
            public_url: &public_url,
            labels: labels.clone(),
        };
        let body = render_service(render_template(&plan), labels, candidate_traffic(&existing_targets, &tag));
        let what = format!("start version {} on Cloud Run", request.version);
        let started = self.patch_service(&body, "", &what).await;
        let service = match started {
            Ok(service) => service,
            Err(error) => {
                let error = self.candidate_failed(error, &tag).await;
                emit(progress, Step::Starting, StepStatus::Failed, error.message.clone());
                return Err(error);
            }
        };
        let Some(revision) =
            service.get("latestReadyRevision").and_then(Value::as_str).map(short_name).filter(|ready| {
                service.get("latestCreatedRevision").and_then(Value::as_str).map(short_name).as_deref()
                    == Some(ready.as_str())
            })
        else {
            let message = failure_message(&service).unwrap_or_else(|| "the new revision did not get ready".to_owned());
            let error = self.candidate_failed(DirectError::new(ErrorCode::HealthCheckFailed, message), &tag).await;
            emit(progress, Step::Starting, StepStatus::Failed, error.message.clone());
            return Err(error);
        };
        emit(progress, Step::Starting, StepStatus::Done, format!("revision {revision} is ready"));
        emit(
            progress,
            Step::Checking,
            StepStatus::Running,
            format!("Moving traffic to {} and checking GET /readyz", request.version),
        );
        let (traffic, dropped) =
            promoted_traffic(&current_targets(&service), &revision, &tag, self.settings.keep_versions);
        let what = format!("move the traffic to version {}", request.version);
        let service = match self.patch_service(&json!({ "traffic": traffic }), "traffic", &what).await {
            Ok(service) => service,
            Err(error) => {
                emit(progress, Step::Checking, StepStatus::Failed, error.message.clone());
                return Err(error);
            }
        };
        let base = self.public_base(&service);
        if let Err(error) = self.wait_public(&base).await {
            emit(progress, Step::Checking, StepStatus::Failed, error.message.clone());
            return Err(error);
        }
        let mut notes = vec![format!("version {} is live", request.version)];
        if !dropped.is_empty() {
            notes.push(format!("{} old version(s) no longer kept", dropped.len()));
        }
        if request.drop_store {
            let objects = self.list_objects(STORE_PREFIX).await?;
            let deleted = self.delete_objects(&objects).await?;
            notes.push(format!("store emptied ({deleted} file(s))"));
        }
        if let Ok(pruned) = self.prune_agents(&service).await
            && pruned > 0
        {
            notes.push(format!("{pruned} unused agent file(s) deleted"));
        }
        emit(progress, Step::Checking, StepStatus::Done, notes.join("; "));
        let store = self.store_summary(request.serve.needs_store()).await?;
        Ok((service, store))
    }

    /// Takes the candidate's tag off the traffic again, so status does not
    /// report a version that never served. The cleanup is best effort.
    async fn candidate_failed(&self, error: DirectError, tag: &str) -> DirectError {
        if let Ok(Some(service)) = self.get_service().await {
            let targets = current_targets(&service);
            if targets.iter().any(|target| target.tag.as_deref() == Some(tag) && target.percent == 0) {
                let body = json!({ "traffic": without_tag(&targets, tag) });
                let _cleanup = self.patch_service(&body, "traffic", "take the failed version off the traffic").await;
            }
        }
        let lower = error.message.to_ascii_lowercase();
        if lower.contains("image '") && lower.contains("not found") {
            return DirectError::new(
                ErrorCode::NotFound,
                format!(
                    "{}. Pass --image with an image that exists (the default is the image of this CLI's graphrt \
                     release, ghcr.io/graphrt/graphrt:{}, published with the release). The previous version (if \
                     any) keeps serving",
                    error.message.trim_end_matches('.'),
                    env!("CARGO_PKG_VERSION")
                ),
            );
        }
        let code = match error.code {
            ErrorCode::StartTimeout
            | ErrorCode::PermissionDenied
            | ErrorCode::ApiDisabled
            | ErrorCode::BillingDisabled
            | ErrorCode::OrgPolicy
            | ErrorCode::Unauthorized
            | ErrorCode::RateLimited
            | ErrorCode::GcpUnavailable => error.code,
            _ => ErrorCode::HealthCheckFailed,
        };
        DirectError::new(
            code,
            format!(
                "{}. The previous version (if any) keeps serving; `gcloud run services logs read {} --project {} \
                 --region {}` shows why",
                error.message.trim_end_matches('.'),
                self.service,
                self.project(),
                self.region()
            ),
        )
    }

    /// Waits for `GET <base>/readyz` to answer 200 from outside.
    async fn wait_public(&self, base: &str) -> Result<(), DirectError> {
        if base.is_empty() {
            return Err(DirectError::new(ErrorCode::GcpApi, "Cloud Run reported no URL for the service"));
        }
        let client = crate::http_client::build_client(crate::http_client::ClientOptions {
            request_timeout: Some(Duration::from_secs(30)),
            bypass_proxy: false,
        })
        .map_err(|error| DirectError::new(ErrorCode::Internal, error.message))?;
        let deadline = tokio::time::Instant::now() + self.settings.health_timeout;
        let mut last: String;
        loop {
            match client.get(format!("{base}/readyz")).send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last = format!("HTTP {}", response.status().as_u16()),
                Err(error) => last = if error.is_timeout() { "timeout".to_owned() } else { "connect error".to_owned() },
            }
            if tokio::time::Instant::now() + self.settings.poll_interval > deadline {
                return Err(DirectError::new(
                    ErrorCode::HealthCheckFailed,
                    format!(
                        "GET {base}/readyz did not pass within {} s ({last}). An organization policy may keep the \
                         service private (constraints/iam.allowedPolicyMemberDomains or run.managed.requireInvokerIam)",
                        self.settings.health_timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(self.settings.poll_interval).await;
        }
    }

    /// Deletes agent folders no kept revision mounts.
    async fn prune_agents(&self, service: &Value) -> Result<usize, DirectError> {
        let revisions = self.revisions().await?;
        let kept: Vec<String> = current_targets(service)
            .iter()
            .filter_map(|target| revisions.get(&target.revision))
            .filter_map(|revision| revision.pointer(&format!("/labels/{AGENT_LABEL}")).and_then(Value::as_str))
            .map(str::to_owned)
            .chain(
                service.pointer(&format!("/template/labels/{AGENT_LABEL}")).and_then(Value::as_str).map(str::to_owned),
            )
            .collect();
        let unused: Vec<String> = self
            .list_objects(AGENTS_PREFIX)
            .await?
            .into_iter()
            .filter(|name| {
                let dir = name.trim_start_matches(AGENTS_PREFIX).split('/').next().unwrap_or_default();
                !kept.iter().any(|kept_dir| kept_dir == dir)
            })
            .collect();
        self.delete_objects(&unused).await
    }

    /// Every revision of the service by short name.
    async fn revisions(&self) -> Result<BTreeMap<String, Value>, DirectError> {
        let url = format!("{}/revisions?pageSize=100", self.service_url());
        let page = self.client.send(Method::Get, &url, Body::Empty, "list the revisions").await?;
        let mut revisions = BTreeMap::new();
        for revision in page.get("revisions").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = revision.get("name").and_then(Value::as_str) {
                revisions.insert(short_name(name), revision.clone());
            }
        }
        Ok(revisions)
    }

    async fn store_summary(&self, in_use: bool) -> Result<Option<StoreSummary>, DirectError> {
        let has_files = !self.list_objects(STORE_PREFIX).await.unwrap_or_default().is_empty();
        Ok((in_use || has_files).then(|| StoreSummary { bucket: self.bucket(), path: STORE_PREFIX.to_owned(), in_use }))
    }

    /// The deployment's state, URL, current version and traffic.
    ///
    /// @throws DirectError(NotFound) - there is no such deployment
    pub async fn status(&self) -> Result<DeploymentStatus, DirectError> {
        let service = self.existing_direct_service().await?;
        let revisions = self.revisions().await?;
        let targets = current_targets(&service);
        let serving = targets.iter().filter(|target| target.percent > 0).max_by_key(|target| target.percent);
        let serving_revision = serving.and_then(|target| revisions.get(&target.revision));
        let label = |revision: Option<&Value>, key: &str| {
            revision
                .and_then(|revision| revision.pointer(&format!("/labels/{key}")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let paused = service.pointer(&format!("/labels/{PAUSED_LABEL}")).and_then(Value::as_str) == Some("1");
        let reconciling = service.get("reconciling").and_then(Value::as_bool) == Some(true);
        let message = failure_message(&service);
        let state = if paused {
            DeploymentState::Paused
        } else if reconciling {
            DeploymentState::Deploying
        } else if serving_revision.is_none() {
            if message.is_some() { DeploymentState::Failed } else { DeploymentState::Offline }
        } else {
            DeploymentState::Online
        };
        let in_use = label(serving_revision, STORE_LABEL).as_deref() == Some("1");
        Ok(DeploymentStatus {
            app: self.service.clone(),
            url: service.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned(),
            status: state,
            current_version: serving_revision.and_then(version_of),
            workflow: label(serving_revision, WORKFLOW_LABEL),
            mode: label(serving_revision, MODE_LABEL),
            last_deployed_at: serving_revision.and_then(create_time),
            traffic: traffic_summary(&targets, |name| revisions.get(name).and_then(version_of)),
            store: self.store_summary(in_use).await?,
            project: self.project().to_owned(),
            region: self.region().to_owned(),
            message,
        })
    }

    /// The deployment's URL.
    ///
    /// @throws DirectError(NotFound) - there is no such deployment
    pub async fn url(&self) -> Result<String, DirectError> {
        let service = self.existing_direct_service().await?;
        Ok(service.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned())
    }

    /// Every version kept for rollback (tagged), newest deploy first.
    pub async fn list_versions(&self) -> Result<Vec<VersionSummary>, DirectError> {
        let service = self.existing_direct_service().await?;
        let revisions = self.revisions().await?;
        let mut versions: Vec<VersionSummary> = current_targets(&service)
            .into_iter()
            .filter(|target| target.tag.is_some() || target.percent > 0)
            .filter_map(|target| {
                let revision = revisions.get(&target.revision)?;
                let label =
                    |key: &str| revision.pointer(&format!("/labels/{key}")).and_then(Value::as_str).map(str::to_owned);
                Some(VersionSummary {
                    label: version_of(revision).unwrap_or_else(|| target.tag.clone().unwrap_or_default()),
                    deployed_at: create_time(revision),
                    current: target.percent > 0,
                    revision: target.revision.clone(),
                    mode: label(MODE_LABEL),
                    workflow: label(WORKFLOW_LABEL),
                    state: if target.percent > 0 { "serving".to_owned() } else { "standby".to_owned() },
                })
            })
            .collect();
        versions.sort_by(|a, b| b.deployed_at.cmp(&a.deployed_at));
        versions.dedup_by(|a, b| a.revision == b.revision);
        Ok(versions)
    }

    /// Moves all traffic back to a kept version.
    ///
    /// @param version - a label from [`CloudRunDirect::list_versions`] (&str)
    /// @throws DirectError(NotFound) - the version is not kept
    pub async fn rollback(&self, version: &str, progress: Progress<'_>) -> Result<ServingVersion, DirectError> {
        let service = self.existing_direct_service().await?;
        let revisions = self.revisions().await?;
        let targets = current_targets(&service);
        let tag = tag_for(version);
        let target = targets
            .iter()
            .find(|target| {
                target.tag.as_deref() == Some(tag.as_str())
                    || revisions.get(&target.revision).and_then(version_of).as_deref() == Some(version)
            })
            .cloned()
            .ok_or_else(|| {
                DirectError::new(ErrorCode::NotFound, format!("version {version} is not kept (see list-versions)"))
            })?;
        let workflow = revisions
            .get(&target.revision)
            .and_then(|revision| revision.pointer(&format!("/labels/{WORKFLOW_LABEL}")))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let label = revisions.get(&target.revision).and_then(version_of).unwrap_or_else(|| version.to_owned());
        emit(progress, Step::Checking, StepStatus::Running, format!("Moving traffic back to {label}"));
        let target_tag = target.tag.clone().unwrap_or(tag);
        let others: Vec<Target> = targets.into_iter().filter(|other| other.revision != target.revision).collect();
        let (traffic, _) =
            promoted_traffic(&others, &target.revision, &target_tag, self.settings.keep_versions.max(others.len() + 1));
        let service =
            match self.patch_service(&json!({ "traffic": traffic }), "traffic", &format!("roll back to {label}")).await
            {
                Ok(service) => service,
                Err(error) => {
                    emit(progress, Step::Checking, StepStatus::Failed, error.message.clone());
                    return Err(error);
                }
            };
        let url = service.get("uri").and_then(Value::as_str).unwrap_or_default().to_owned();
        if !is_paused(&service)
            && let Err(error) = self.wait_public(&self.public_base(&service)).await
        {
            emit(progress, Step::Checking, StepStatus::Failed, error.message.clone());
            return Err(error);
        }
        emit(progress, Step::Checking, StepStatus::Done, format!("version {label} serves again"));
        Ok(ServingVersion { app: self.service.clone(), url, version: label, workflow })
    }

    /// Takes the deployment off the internet: internal ingress, invoker IAM
    /// checked again, no instances. `false` when it was paused already.
    pub async fn pause(&self) -> Result<bool, DirectError> {
        let service = self.existing_direct_service().await?;
        if is_paused(&service) {
            return Ok(false);
        }
        let mut labels = service.get("labels").cloned().unwrap_or_else(|| json!({}));
        if let Some(object) = labels.as_object_mut() {
            object.insert(PAUSED_LABEL.to_owned(), Value::from("1"));
        }
        let body = json!({
            "labels": labels,
            "ingress": "INGRESS_TRAFFIC_INTERNAL_ONLY",
            "invokerIamDisabled": false,
            "scaling": { "scalingMode": "MANUAL", "manualInstanceCount": 0 },
        });
        self.patch_service(&body, "labels,ingress,invokerIamDisabled,scaling", "pause the deployment").await?;
        Ok(true)
    }

    /// Brings a paused deployment back. `None` when it was not paused.
    pub async fn resume(&self, progress: Progress<'_>) -> Result<Option<ServingVersion>, DirectError> {
        let service = self.existing_direct_service().await?;
        if !is_paused(&service) {
            return Ok(None);
        }
        emit(
            progress,
            Step::Checking,
            StepStatus::Running,
            "Putting the deployment back online and checking GET /readyz",
        );
        let mut labels = service.get("labels").cloned().unwrap_or_else(|| json!({}));
        if let Some(object) = labels.as_object_mut() {
            object.remove(PAUSED_LABEL);
        }
        let body = json!({
            "labels": labels,
            "ingress": "INGRESS_TRAFFIC_ALL",
            "invokerIamDisabled": true,
            "scaling": { "scalingMode": "AUTOMATIC" },
        });
        let service =
            self.patch_service(&body, "labels,ingress,invokerIamDisabled,scaling", "resume the deployment").await?;
        if let Err(error) = self.wait_public(&self.public_base(&service)).await {
            emit(progress, Step::Checking, StepStatus::Failed, error.message.clone());
            return Err(error);
        }
        let status = self.status().await?;
        emit(progress, Step::Checking, StepStatus::Done, "online again");
        Ok(Some(ServingVersion {
            app: self.service.clone(),
            url: status.url,
            version: status.current_version.unwrap_or_default(),
            workflow: status.workflow,
        }))
    }

    /// Deletes the service, its secrets, its bucket (with the store) and its
    /// service account. The shared remote repository stays.
    ///
    /// @returns What was deleted (`removed` is false when there was no service)
    pub async fn remove(&self) -> Result<RemoveOutcome, DirectError> {
        let service = self.get_service().await?;
        if let Some(service) = &service {
            check_ours(service, &self.service)?;
            let operation = self
                .client
                .call(Method::Delete, &self.service_url(), Body::Empty)
                .await
                .map_err(|error| error.to_direct("delete the Cloud Run service", &self.client.context))?;
            let run = self.client.endpoints.run.clone();
            self.client
                .wait_operation(
                    &run,
                    "v2",
                    operation,
                    self.settings.start_timeout,
                    self.settings.poll_interval,
                    "delete the Cloud Run service",
                )
                .await?;
        }
        let secrets = self.delete_secrets().await?;
        let bucket = self.delete_bucket().await?;
        let account = self.delete_service_account().await?;
        Ok(RemoveOutcome {
            removed: service.is_some(),
            secrets_deleted: secrets,
            bucket_deleted: bucket,
            service_account_deleted: account,
        })
    }
}

/// What `remove` deleted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RemoveOutcome {
    /// The Cloud Run service existed and was deleted.
    pub removed: bool,
    pub secrets_deleted: usize,
    pub bucket_deleted: bool,
    pub service_account_deleted: bool,
}

fn is_paused(service: &Value) -> bool {
    service.pointer(&format!("/labels/{PAUSED_LABEL}")).and_then(Value::as_str) == Some("1")
}

fn version_of(revision: &Value) -> Option<String> {
    let annotation = format!("/annotations/{}", VERSION_ANNOTATION.replace('/', "~1"));
    revision.pointer(&annotation).and_then(Value::as_str).map(str::to_owned)
}

/// `createTime` as RFC 3339 with milliseconds (`2026-11-28T10:00:00.123Z`).
fn create_time(revision: &Value) -> Option<String> {
    let text = revision.get("createTime").and_then(Value::as_str)?;
    let (seconds, rest) = text.split_once('.').unwrap_or((text.trim_end_matches('Z'), "000Z"));
    let millis: String = rest.trim_end_matches('Z').chars().chain(std::iter::repeat('0')).take(3).collect();
    Some(format!("{seconds}.{millis}Z"))
}

/// The agent's folder: 16 hex digits of its SHA-256, so a redeploy of the same
/// agent uploads nothing and each version keeps its own file.
fn agent_dir_of(contents: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, contents);
    digest.as_ref().iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// `<workflow>.<extension>`: `graphrt serve` names the workflow after the file.
fn agent_object_name(file_name: &str, workflow: &str) -> String {
    let extension = file_name.rsplit_once('.').map_or("yaml", |(_, extension)| extension).to_ascii_lowercase();
    format!("{workflow}.{extension}")
}

/// The published image of this graphrt version.
pub(super) fn default_image() -> String {
    format!("{IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// Checks the request before any call to Google.
fn validate_request(request: &DeployRequest) -> Result<(), DirectError> {
    let version = &request.version;
    if version.is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
        return Err(DirectError::invalid("--version must be 1-128 printable characters"));
    }
    if request.agent.contents.is_empty() {
        return Err(DirectError::invalid(format!("{} is empty", request.agent.file_name)));
    }
    if let Some(bad) = request.secrets.keys().find(|name| !is_env_name(name)) {
        return Err(DirectError::invalid(format!("secret name {bad:?} is not a valid environment variable name")));
    }
    if request.secrets.contains_key(API_KEY_ENV) {
        return Err(DirectError::invalid(format!("{API_KEY_ENV} is managed by graphrt deploy (use --rotate-api-key)")));
    }
    request.serve.validate().map_err(DirectError::invalid)?;
    if request.drop_store && request.serve.needs_store() {
        return Err(DirectError::invalid(
            "--drop-store deletes the store this deploy needs: leave out --monitor, --devices, --push, \
             --conversations and --channel, or leave out --drop-store (an agent with `long_term_memory` keeps \
             its notes on the store too: it always needs one)",
        ));
    }
    let scale_to_zero = request.mode == graphrt_provider_api::direct::ServiceMode::ScaleToZero;
    if scale_to_zero && request.serve.channels.contains(&graphrt_provider_api::direct::DirectChannel::Telegram) {
        return Err(DirectError::invalid(
            "--channel telegram needs --mode always-on: an instance that scaled to zero does not ask Telegram for new messages",
        ));
    }
    if scale_to_zero
        && request.serve.channels.contains(&graphrt_provider_api::direct::DirectChannel::Slack)
        && request.secrets.contains_key("SLACK_APP_TOKEN")
    {
        return Err(DirectError::invalid(
            "--channel slack with SLACK_APP_TOKEN (Socket Mode) needs --mode always-on. Or leave out SLACK_APP_TOKEN \
             and use the Events API with SLACK_SIGNING_SECRET",
        ));
    }
    Ok(())
}

fn is_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !matches!(name, "PORT" | "K_SERVICE" | "K_REVISION" | "K_CONFIGURATION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_folders_and_files_are_stable() {
        assert_eq!(agent_dir_of(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(agent_object_name("Support Bot.GRT", "Support-Bot"), "Support-Bot.grt");
        assert_eq!(default_image(), format!("ghcr.io/graphrt/graphrt:{}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn create_times_get_milliseconds() {
        assert_eq!(
            create_time(&json!({"createTime": "2026-11-28T10:00:00.123456Z"})).as_deref(),
            Some("2026-11-28T10:00:00.123Z")
        );
        assert_eq!(
            create_time(&json!({"createTime": "2026-11-28T10:00:00Z"})).as_deref(),
            Some("2026-11-28T10:00:00.000Z")
        );
    }

    #[test]
    fn version_labels_come_from_the_annotation() {
        let revision = json!({"annotations": {"graphrt.dev/version": "2026.11.28 rc"}});
        assert_eq!(version_of(&revision).as_deref(), Some("2026.11.28 rc"));
    }
}
