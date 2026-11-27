//! The project resources of a direct deployment besides the Cloud Run
//! service: the APIs it needs, its bucket and the objects in it, the service
//! account it runs as, its secrets, and the remote repository its image is
//! pulled through.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::google::{Body, GoogleClient, Method};
use super::naming::{bucket_name_for, label_value, secret_id_for, service_account_id_for};
use super::{CloudRunDirect, DirectError, ErrorCode, GHCR_REMOTE_REPOSITORY};
use crate::url_encoding::encode_component;

/// The APIs a direct deploy calls, besides Service Usage itself.
pub(super) const REQUIRED_APIS: [&str; 5] = [
    "run.googleapis.com",
    "storage.googleapis.com",
    "secretmanager.googleapis.com",
    "iam.googleapis.com",
    "artifactregistry.googleapis.com",
];
/// Longest wait for APIs that were just turned on.
const ENABLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest wait for a new service account to be usable in IAM policies.
const ACCOUNT_PROPAGATION: Duration = Duration::from_secs(90);
/// Annotation of a secret that names the environment variable it fills.
pub(super) const ENV_ANNOTATION: &str = "graphrt-env";
/// Folder of the agent files in the bucket.
pub(super) const AGENTS_PREFIX: &str = "agents/";
/// Folder of the store in the bucket.
pub(super) const STORE_PREFIX: &str = "store/";

/// A secret of the deployment, as Secret Manager lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DeploymentSecret {
    /// `projects/<p>/secrets/<id>`.
    pub(super) name: String,
    /// Secret id.
    pub(super) id: String,
    /// The environment variable it fills.
    pub(super) env: String,
}

impl CloudRunDirect {
    fn api(&self) -> &GoogleClient {
        &self.client
    }

    pub(super) fn project(&self) -> &str {
        &self.settings.project
    }

    pub(super) fn region(&self) -> &str {
        &self.settings.region
    }

    pub(super) fn bucket(&self) -> String {
        bucket_name_for(self.project(), &self.service)
    }

    pub(super) fn service_account_email(&self) -> String {
        format!("{}@{}.iam.gserviceaccount.com", service_account_id_for(&self.service), self.project())
    }

    /// Labels of every resource: `managed-by`, the deployment and the caller's own.
    pub(super) fn labels(&self, deployment: &str) -> Value {
        let mut labels: BTreeMap<String, String> = self.settings.labels.clone();
        labels.insert("managed-by".to_owned(), "graphrt".to_owned());
        labels.insert("graphrt-direct".to_owned(), "1".to_owned());
        labels.insert("graphrt-service".to_owned(), label_value(&self.service));
        labels.insert("graphrt-deployment".to_owned(), label_value(deployment));
        json!(labels)
    }

    /// Turns on the APIs the deploy needs that are off.
    ///
    /// @returns The project number (from the API list), when Google names it
    pub(super) async fn ensure_apis(&self) -> Result<Option<String>, DirectError> {
        let base = &self.api().endpoints.service_usage;
        let project = self.project();
        let url = format!("{base}/v1/projects/{project}/services?filter=state:ENABLED&pageSize=200");
        let mut enabled: Vec<String> = Vec::new();
        let mut number = None;
        let mut page_url = url.clone();
        loop {
            let page = self.api().send(Method::Get, &page_url, Body::Empty, "list the APIs that are on").await?;
            for service in page.get("services").and_then(Value::as_array).into_iter().flatten() {
                if let Some(name) = service.pointer("/config/name").and_then(Value::as_str) {
                    enabled.push(name.to_owned());
                }
                if let Some(parent) =
                    service.get("parent").and_then(Value::as_str).and_then(|p| p.strip_prefix("projects/"))
                {
                    number = Some(parent.to_owned());
                }
            }
            match page.get("nextPageToken").and_then(Value::as_str).filter(|token| !token.is_empty()) {
                Some(token) => page_url = format!("{url}&pageToken={}", encode_component(token)),
                None => break,
            }
        }
        let missing: Vec<String> = REQUIRED_APIS
            .iter()
            .filter(|api| !enabled.iter().any(|name| name == *api))
            .map(|api| (*api).to_owned())
            .collect();
        if missing.is_empty() {
            return Ok(number);
        }
        let what = format!("turn on {}", missing.join(", "));
        let enable_url = format!("{base}/v1/projects/{project}/services:batchEnable");
        let operation =
            self.api().send(Method::Post, &enable_url, Body::Json(&json!({ "serviceIds": missing })), &what).await?;
        self.api().wait_operation(base, "v1", operation, ENABLE_TIMEOUT, self.settings.poll_interval, &what).await?;
        Ok(number)
    }

    /// Creates the deployment's service account when it is missing.
    pub(super) async fn ensure_service_account(&self, deployment: &str) -> Result<String, DirectError> {
        let base = &self.api().endpoints.iam;
        let email = self.service_account_email();
        let project = self.project();
        let url = format!("{base}/v1/projects/{project}/serviceAccounts/{email}");
        if self.api().get_optional(&url, "read the service account").await?.is_some() {
            return Ok(email);
        }
        let body = json!({
            "accountId": service_account_id_for(&self.service),
            "serviceAccount": {
                "displayName": format!("graphrt {}", self.service),
                "description": format!("Runs the graphrt deployment {deployment} (Cloud Run service {})", self.service),
            },
        });
        let create = format!("{base}/v1/projects/{project}/serviceAccounts");
        match self.api().call(Method::Post, &create, Body::Json(&body)).await {
            Ok(_) => Ok(email),
            Err(error) if error.is_conflict() => Ok(email),
            Err(error) => Err(error.to_direct(&format!("create the service account {email}"), &self.api().context)),
        }
    }

    /// Deletes the deployment's service account. `false` when it did not exist.
    pub(super) async fn delete_service_account(&self) -> Result<bool, DirectError> {
        let email = self.service_account_email();
        let url = format!("{}/v1/projects/{}/serviceAccounts/{email}", self.api().endpoints.iam, self.project());
        self.delete_optional(&url, &format!("delete the service account {email}")).await
    }

    /// Creates the bucket when missing.
    ///
    /// @throws DirectError(NameTaken) - another project owns the name
    pub(super) async fn ensure_bucket(&self, deployment: &str) -> Result<String, DirectError> {
        let base = &self.api().endpoints.storage;
        let bucket = self.bucket();
        let url = format!("{base}/storage/v1/b/{bucket}");
        if let Some(existing) = self.api().get_optional(&url, &format!("read the bucket {bucket}")).await? {
            let ours = existing.pointer("/labels/managed-by").and_then(Value::as_str) == Some("graphrt");
            if !ours {
                return Err(name_taken_bucket(&bucket));
            }
            return Ok(bucket);
        }
        let body = json!({
            "name": bucket,
            "location": self.region(),
            "storageClass": "STANDARD",
            "iamConfiguration": {
                "uniformBucketLevelAccess": { "enabled": true },
                "publicAccessPrevention": "enforced",
            },
            "labels": self.labels(deployment),
        });
        let create = format!("{base}/storage/v1/b?project={}", self.project());
        match self.api().call(Method::Post, &create, Body::Json(&body)).await {
            Ok(_) => Ok(bucket),
            // 409 when the name is taken, by this project too: read it again to tell.
            Err(error) if error.is_conflict() => match self.api().get_optional(&url, "read the bucket").await? {
                Some(existing) if existing.pointer("/labels/managed-by").and_then(Value::as_str) == Some("graphrt") => {
                    Ok(bucket)
                }
                _ => Err(name_taken_bucket(&bucket)),
            },
            Err(error) => {
                Err(error.to_direct(&format!("create the Cloud Storage bucket {bucket}"), &self.api().context))
            }
        }
    }

    /// Gives `member` `role` on the bucket (read, change, write back).
    pub(super) async fn grant_bucket(&self, member: &str, role: &str) -> Result<(), DirectError> {
        let bucket = self.bucket();
        let url = format!("{}/storage/v1/b/{bucket}/iam", self.api().endpoints.storage);
        let what = format!("let {member} use the bucket {bucket}");
        let policy = self.api().send(Method::Get, &url, Body::Empty, &what).await?;
        let Some(updated) = with_binding(&policy, role, member) else { return Ok(()) };
        self.retry_new_account(|| self.api().call(Method::Put, &url, Body::Json(&updated)), &what).await.map(drop)
    }

    /// Uploads `contents` as `object` unless an object of that name exists (names carry the SHA-256).
    ///
    /// @returns Whether anything was uploaded
    pub(super) async fn upload_object(&self, object: &str, contents: &[u8]) -> Result<bool, DirectError> {
        let base = &self.api().endpoints.storage;
        let bucket = self.bucket();
        let encoded = encode_component(object);
        let url = format!("{base}/storage/v1/b/{bucket}/o/{encoded}");
        if self.api().get_optional(&url, "read the agent file in the bucket").await?.is_some() {
            return Ok(false);
        }
        let upload = format!("{base}/upload/storage/v1/b/{bucket}/o?uploadType=media&name={encoded}");
        let what = format!("upload the agent to gs://{bucket}/{object}");
        self.api().send(Method::Post, &upload, Body::Bytes("application/octet-stream", contents), &what).await?;
        Ok(true)
    }

    /// Object names under `prefix`.
    pub(super) async fn list_objects(&self, prefix: &str) -> Result<Vec<String>, DirectError> {
        let base = &self.api().endpoints.storage;
        let bucket = self.bucket();
        let url = format!(
            "{base}/storage/v1/b/{bucket}/o?prefix={}&fields=items/name,nextPageToken",
            encode_component(prefix)
        );
        let mut names = Vec::new();
        let mut page_url = url.clone();
        loop {
            let Some(page) = self.api().get_optional(&page_url, &format!("list gs://{bucket}/{prefix}")).await? else {
                return Ok(names);
            };
            for item in page.get("items").and_then(Value::as_array).into_iter().flatten() {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    names.push(name.to_owned());
                }
            }
            match page.get("nextPageToken").and_then(Value::as_str).filter(|token| !token.is_empty()) {
                Some(token) => page_url = format!("{url}&pageToken={}", encode_component(token)),
                None => return Ok(names),
            }
        }
    }

    /// Deletes objects; one that is already gone is fine.
    pub(super) async fn delete_objects(&self, names: &[String]) -> Result<usize, DirectError> {
        let base = &self.api().endpoints.storage;
        let bucket = self.bucket();
        let mut deleted = 0;
        for name in names {
            let url = format!("{base}/storage/v1/b/{bucket}/o/{}", encode_component(name));
            if self.delete_optional(&url, &format!("delete gs://{bucket}/{name}")).await? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Empties and deletes the bucket. `false` when it did not exist.
    pub(super) async fn delete_bucket(&self) -> Result<bool, DirectError> {
        let bucket = self.bucket();
        let url = format!("{}/storage/v1/b/{bucket}", self.api().endpoints.storage);
        let Some(existing) = self.api().get_optional(&url, &format!("read the bucket {bucket}")).await? else {
            return Ok(false);
        };
        if existing.pointer("/labels/managed-by").and_then(Value::as_str) != Some("graphrt") {
            return Ok(false);
        }
        let objects = self.list_objects("").await?;
        self.delete_objects(&objects).await?;
        self.delete_optional(&url, &format!("delete the bucket {bucket}")).await
    }

    /// The deployment's secrets (labelled with its service).
    pub(super) async fn list_secrets(&self) -> Result<Vec<DeploymentSecret>, DirectError> {
        let base = &self.api().endpoints.secret_manager;
        let filter = encode_component(&format!("labels.graphrt-service={}", label_value(&self.service)));
        let url = format!("{base}/v1/projects/{}/secrets?filter={filter}&pageSize=250", self.project());
        let page = self.api().send(Method::Get, &url, Body::Empty, "list the deployment's secrets").await?;
        let mut secrets = Vec::new();
        for secret in page.get("secrets").and_then(Value::as_array).into_iter().flatten() {
            let name = secret.get("name").and_then(Value::as_str).unwrap_or_default();
            let env = secret.pointer(&format!("/annotations/{ENV_ANNOTATION}")).and_then(Value::as_str);
            if let (Some(id), Some(env)) = (name.rsplit('/').next().filter(|id| !id.is_empty()), env) {
                secrets.push(DeploymentSecret { name: name.to_owned(), id: id.to_owned(), env: env.to_owned() });
            }
        }
        Ok(secrets)
    }

    /// Stores `value` as the newest version of the secret that fills `env`
    /// (creating the secret when missing) and lets the service account read it.
    pub(super) async fn put_secret(&self, deployment: &str, env: &str, value: &str) -> Result<String, DirectError> {
        let base = &self.api().endpoints.secret_manager;
        let project = self.project();
        let id = secret_id_for(&self.service, env);
        let what = format!("store the secret {env} in Secret Manager");
        let body = json!({
            "replication": { "automatic": {} },
            "labels": self.labels(deployment),
            "annotations": { ENV_ANNOTATION: env },
        });
        let create = format!("{base}/v1/projects/{project}/secrets?secretId={id}");
        match self.api().call(Method::Post, &create, Body::Json(&body)).await {
            Ok(_) => {}
            Err(error) if error.is_conflict() => {}
            Err(error) => return Err(error.to_direct(&what, &self.api().context)),
        }
        let add = format!("{base}/v1/projects/{project}/secrets/{id}:addVersion");
        let payload = json!({ "payload": { "data": STANDARD.encode(value.as_bytes()) } });
        self.api().send(Method::Post, &add, Body::Json(&payload), &what).await?;
        self.grant_secret(&id).await?;
        Ok(id)
    }

    /// Whether the secret `id` has a version that can be read.
    pub(super) async fn secret_has_version(&self, id: &str) -> Result<bool, DirectError> {
        let url = format!(
            "{}/v1/projects/{}/secrets/{id}/versions?filter=state:ENABLED&pageSize=1",
            self.api().endpoints.secret_manager,
            self.project()
        );
        let page = self.api().get_optional(&url, "read the API key secret").await?;
        Ok(page
            .and_then(|page| page.get("versions").and_then(Value::as_array).map(|versions| !versions.is_empty()))
            .unwrap_or(false))
    }

    /// Lets the service account read the secret `id` (`roles/secretmanager.secretAccessor`).
    async fn grant_secret(&self, id: &str) -> Result<(), DirectError> {
        let base = &self.api().endpoints.secret_manager;
        let resource = format!("{base}/v1/projects/{}/secrets/{id}", self.project());
        let member = format!("serviceAccount:{}", self.service_account_email());
        let what = format!("let the service account read the secret {id}");
        let policy = self.api().send(Method::Get, &format!("{resource}:getIamPolicy"), Body::Empty, &what).await?;
        let Some(updated) = with_binding(&policy, "roles/secretmanager.secretAccessor", &member) else { return Ok(()) };
        let body = json!({ "policy": updated });
        let url = format!("{resource}:setIamPolicy");
        self.retry_new_account(|| self.api().call(Method::Post, &url, Body::Json(&body)), &what).await.map(drop)
    }

    /// Deletes the deployment's secrets; the number deleted.
    pub(super) async fn delete_secrets(&self) -> Result<usize, DirectError> {
        let mut deleted = 0;
        for secret in self.list_secrets().await? {
            let url = format!("{}/v1/{}", self.api().endpoints.secret_manager, secret.name);
            if self.delete_optional(&url, &format!("delete the secret {}", secret.id)).await? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// The image a revision runs: `image` as given, except that `ghcr.io/...`
    /// goes through the project's Artifact Registry remote repository (created
    /// when missing), since Cloud Run pulls only from Artifact Registry, Container
    /// Registry and Docker Hub.
    pub(super) async fn resolve_image(&self, image: &str) -> Result<String, DirectError> {
        let Some(path) = image.strip_prefix("ghcr.io/") else { return Ok(image.to_owned()) };
        self.ensure_ghcr_remote().await?;
        Ok(format!("{}-docker.pkg.dev/{}/{GHCR_REMOTE_REPOSITORY}/{path}", self.region(), self.project()))
    }

    async fn ensure_ghcr_remote(&self) -> Result<(), DirectError> {
        let base = &self.api().endpoints.artifact_registry;
        let parent = format!("projects/{}/locations/{}", self.project(), self.region());
        let url = format!("{base}/v1/{parent}/repositories/{GHCR_REMOTE_REPOSITORY}");
        let what = format!("create the Artifact Registry remote repository {GHCR_REMOTE_REPOSITORY} for ghcr.io");
        if self.api().get_optional(&url, &what).await?.is_some() {
            return Ok(());
        }
        let mut labels = self.settings.labels.clone();
        labels.insert("managed-by".to_owned(), "graphrt".to_owned());
        let body = json!({
            "format": "DOCKER",
            "mode": "REMOTE_REPOSITORY",
            "description": "Pull through cache of ghcr.io for graphrt deploy --direct (the published graphrt image)",
            "labels": labels,
            "remoteRepositoryConfig": {
                "description": "ghcr.io",
                "dockerRepository": { "customRepository": { "uri": "https://ghcr.io" } },
            },
        });
        let create = format!("{base}/v1/{parent}/repositories?repositoryId={GHCR_REMOTE_REPOSITORY}");
        match self.api().call(Method::Post, &create, Body::Json(&body)).await {
            Ok(operation) => {
                self.api()
                    .wait_operation(base, "v1", operation, Duration::from_secs(120), self.settings.poll_interval, &what)
                    .await?;
                Ok(())
            }
            Err(error) if error.is_conflict() => Ok(()),
            Err(error) => Err(error.to_direct(&what, &self.api().context)),
        }
    }

    /// `DELETE url`; `false` on 404.
    pub(super) async fn delete_optional(&self, url: &str, what: &str) -> Result<bool, DirectError> {
        match self.api().call(Method::Delete, url, Body::Empty).await {
            Ok(_) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error.to_direct(what, &self.api().context)),
        }
    }

    /// Runs `call` until IAM knows the service account created a moment ago
    /// (a policy naming it fails with 400 "does not exist" for a few seconds).
    async fn retry_new_account<F, Fut>(&self, call: F, what: &str) -> Result<Value, DirectError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Value, Box<super::google::ApiError>>>,
    {
        let deadline = tokio::time::Instant::now() + ACCOUNT_PROPAGATION;
        let mut pause = self.settings.poll_interval;
        loop {
            match call().await {
                Ok(value) => return Ok(value),
                Err(error)
                    if error.status == 400
                        && error.message.to_ascii_lowercase().contains("does not exist")
                        && tokio::time::Instant::now() + pause < deadline =>
                {
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(Duration::from_secs(10));
                }
                Err(error) => return Err(error.to_direct(what, &self.api().context)),
            }
        }
    }
}

fn name_taken_bucket(bucket: &str) -> DirectError {
    DirectError::new(
        ErrorCode::NameTaken,
        format!(
            "the Cloud Storage bucket name {bucket} is taken by another project or was not created by graphrt: \
             pass another --app"
        ),
    )
}

/// `policy` with `member` added to `role`; `None` when it is there already.
fn with_binding(policy: &Value, role: &str, member: &str) -> Option<Value> {
    let mut policy = if policy.is_object() { policy.clone() } else { json!({}) };
    let object = policy.as_object_mut()?;
    let bindings = object.entry("bindings").or_insert_with(|| json!([]));
    let list = bindings.as_array_mut()?;
    if let Some(binding) = list.iter_mut().find(|binding| binding.get("role").and_then(Value::as_str) == Some(role)) {
        let members = binding.as_object_mut()?.entry("members").or_insert_with(|| json!([]));
        let members = members.as_array_mut()?;
        if members.iter().any(|existing| existing.as_str() == Some(member)) {
            return None;
        }
        members.push(Value::from(member));
    } else {
        list.push(json!({ "role": role, "members": [member] }));
    }
    Some(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_are_added_once_and_keep_the_etag() {
        let policy = json!({ "etag": "BwX", "bindings": [{ "role": "roles/viewer", "members": ["user:a@b.co"] }] });
        let updated = with_binding(&policy, "roles/storage.objectUser", "serviceAccount:s@p.iam.gserviceaccount.com")
            .unwrap_or_default();
        assert_eq!(updated["etag"], "BwX");
        assert_eq!(updated["bindings"].as_array().map(Vec::len), Some(2));
        assert!(
            with_binding(&updated, "roles/storage.objectUser", "serviceAccount:s@p.iam.gserviceaccount.com").is_none()
        );
        let empty = with_binding(&json!({"etag": "x"}), "roles/r", "user:u").unwrap_or_default();
        assert_eq!(empty["bindings"][0]["members"][0], "user:u");
    }
}
