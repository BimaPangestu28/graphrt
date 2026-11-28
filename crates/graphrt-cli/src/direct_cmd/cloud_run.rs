//! `graphrt deploy --target cloud-run --direct` (feature `deploy-cloud-run`):
//! the Google Cloud Run side of [`super`], see `docs/deploy-cloud-run.md`.
//!
//! Credentials, in order: an access token in `GOOGLE_OAUTH_ACCESS_TOKEN` (what
//! an app that signed the person in with Google hands over), the file named by
//! `GOOGLE_APPLICATION_CREDENTIALS` (a service-account key or user
//! credentials), the application default credentials of `gcloud auth
//! application-default login`. The project is never guessed from gcloud's
//! configuration: `--project` or `GOOGLE_CLOUD_PROJECT`.

use std::collections::BTreeMap;

use graphrt_gcp::auth::{ACCESS_TOKEN_ENV, GcpAuth};
use graphrt_gcp::direct::{
    CloudRunDirect, DEFAULT_REGION, DeployRequest, DirectError, DirectSettings, Endpoints, service_name_for,
};
use graphrt_provider_api::direct::StepEvent;
use serde_json::{Value, json};

use super::{Failure, Reporter, common_deploy, runtime};
use crate::deploy_cmd::{DeployAction, DeployRunArgs};

/// Every Google API at this base URL instead (tests against a fake Google).
const API_URL_ENV: &str = "GRAPHRT_GCP_API_URL";
/// The project when `--project` is not given (the variable Google's tools read).
const PROJECT_ENV: &str = "GOOGLE_CLOUD_PROJECT";

impl From<DirectError> for Failure {
    fn from(error: DirectError) -> Self {
        Self::new(error.code.as_str(), error.message)
    }
}

/// `graphrt deploy --target cloud-run --direct ... <agent>`.
pub(super) fn run_deploy(args: &DeployRunArgs, reporter: &mut Reporter) -> Result<(), Failure> {
    if args.org.is_some() {
        return Err(Failure::invalid("--org is a Fly option: Cloud Run takes --project"));
    }
    let labels = parse_labels(&args.labels)?;
    let common = common_deploy(args, reporter)?;
    let direct =
        direct_for(&common.deployment, args.app.as_deref(), args.project.as_deref(), args.region.as_deref(), labels)?;
    let request = DeployRequest {
        deployment: common.deployment,
        version: common.version,
        mode: common.mode,
        image: args.image.clone(),
        secrets: common.secrets,
        api_key_candidate: common.api_key_candidate,
        rotate_api_key: args.rotate_api_key,
        agent: common.agent,
        serve: common.serve,
        drop_store: args.serve.drop_store,
    };
    let outcome = runtime()?
        .block_on(direct.deploy(&request, &mut |event| reporter.step(&event)))
        .map_err(|failure| Failure { api_key: failure.api_key, ..Failure::from(failure.error) })?;
    let fields = json!({
        "action": "deploy", "url": outcome.url, "version": outcome.version, "app": outcome.app,
        "workflow": outcome.workflow, "serve": outcome.serve, "store": outcome.store,
        "project": outcome.project, "region": outcome.region, "image": outcome.image,
    });
    let mut human = format!(
        "deployed {} to {} (POST {}/v1/workflows/{}/runs)",
        outcome.version, outcome.url, outcome.url, outcome.workflow
    );
    if outcome.serve.chat.is_some() {
        human.push_str(&format!("\nchat page: {}/chat/", outcome.url));
    }
    if outcome.serve.monitor {
        human.push_str(&format!("\nmonitoring page: {}/monitor/", outcome.url));
    }
    if let Some(store) = &outcome.store {
        let note = if store.in_use { "in use" } else { "kept, not used by this version: --drop-store empties it" };
        human.push_str(&format!("\nstore: gs://{}/{} ({note})", store.bucket, store.path));
    }
    reporter.result(fields, outcome.api_key.as_deref(), &human);
    Ok(())
}

/// `graphrt deploy status|list-versions|rollback|pause|resume|remove --target cloud-run --direct`.
pub(super) fn run_action(action: &DeployAction, reporter: &Reporter) -> Result<(), Failure> {
    let target = action.target();
    if target.org.is_some() {
        return Err(Failure::invalid("--org is a Fly option: Cloud Run takes --project"));
    }
    let direct = direct_for(
        &target.deployment,
        target.app.as_deref(),
        target.project.as_deref(),
        target.region.as_deref(),
        BTreeMap::new(),
    )?;
    let (project, region) = direct.location();
    let (project, region) = (project.to_owned(), region.to_owned());
    let app = direct.service_name().to_owned();
    let runtime = runtime()?;
    let mut progress = |event: StepEvent| reporter.step(&event);
    let (fields, human) = match action {
        DeployAction::Status(_) => {
            let status = runtime.block_on(direct.status())?;
            let human = format!(
                "{}: {} ({}), version {}, deployed {}",
                status.app,
                serde_json::to_value(status.status).unwrap_or(Value::Null).as_str().unwrap_or("unknown"),
                status.url,
                status.current_version.as_deref().unwrap_or("-"),
                status.last_deployed_at.as_deref().unwrap_or("-")
            );
            (serde_json::to_value(&status).unwrap_or(Value::Null), human)
        }
        DeployAction::ListVersions(_) => {
            let (url, versions) = runtime
                .block_on(async { Ok::<_, DirectError>((direct.url().await?, direct.list_versions().await?)) })?;
            let human = versions
                .iter()
                .map(|entry| {
                    let marker = if entry.current { "*" } else { " " };
                    format!(
                        "{marker} {}  {}  {}",
                        entry.label,
                        entry.deployed_at.as_deref().unwrap_or("-"),
                        entry.state
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let current_version = versions.iter().find(|entry| entry.current).map(|entry| entry.label.clone());
            let fields = json!({
                "app": app, "url": url, "current_version": current_version, "versions": versions,
                "project": project, "region": region,
            });
            (fields, human)
        }
        DeployAction::Rollback { version, .. } => {
            let serving = runtime.block_on(direct.rollback(version, &mut progress))?;
            let human = format!("{} now serves {}", serving.url, serving.version);
            (serde_json::to_value(&serving).unwrap_or(Value::Null), human)
        }
        DeployAction::Pause(_) => {
            let (changed, url) =
                runtime.block_on(async { Ok::<_, DirectError>((direct.pause().await?, direct.url().await?)) })?;
            let human = format!("{app} paused{}", if changed { "" } else { " (already)" });
            (json!({"app": app, "url": url, "status": "paused", "changed": changed}), human)
        }
        DeployAction::Resume(_) => {
            let (serving, url) = runtime
                .block_on(async { Ok::<_, DirectError>((direct.resume(&mut progress).await?, direct.url().await?)) })?;
            let version = serving.as_ref().map(|serving| serving.version.clone());
            let human = format!("{url} online{}", if serving.is_some() { "" } else { " (was not paused)" });
            let fields = json!({
                "app": app, "url": url, "status": "online", "version": version, "changed": serving.is_some(),
            });
            (fields, human)
        }
        DeployAction::Remove(_) => {
            let removed = runtime.block_on(direct.remove())?;
            let human = if removed.removed {
                format!(
                    "{app} removed (service, {} secret(s), bucket {}, service account {})",
                    removed.secrets_deleted,
                    if removed.bucket_deleted { "deleted" } else { "not found" },
                    if removed.service_account_deleted { "deleted" } else { "not found" }
                )
            } else {
                format!("{app} did not exist (left over resources cleaned: {} secret(s))", removed.secrets_deleted)
            };
            let mut fields = serde_json::to_value(&removed).unwrap_or(Value::Null);
            if let Some(object) = fields.as_object_mut() {
                object.insert("app".to_owned(), Value::from(app.as_str()));
            }
            (fields, human)
        }
    };
    let mut fields = fields;
    if let Some(object) = fields.as_object_mut() {
        object.insert("action".to_owned(), Value::from(action.name()));
    }
    reporter.result(fields, None, &human);
    Ok(())
}

/// `--label KEY=VALUE` flags.
fn parse_labels(flags: &[String]) -> Result<BTreeMap<String, String>, Failure> {
    flags
        .iter()
        .map(|flag| {
            flag.split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .ok_or_else(|| Failure::invalid(format!("--label takes KEY=VALUE, got `{flag}`")))
        })
        .collect()
}

/// The deployer of a deployment's service, with the caller's Google credentials.
fn direct_for(
    deployment: &str,
    app: Option<&str>,
    project: Option<&str>,
    region: Option<&str>,
    labels: BTreeMap<String, String>,
) -> Result<CloudRunDirect, Failure> {
    let project = project
        .map(str::to_owned)
        .or_else(|| std::env::var(PROJECT_ENV).ok().filter(|project| !project.is_empty()))
        .ok_or_else(|| {
            Failure::invalid(format!(
                "--target cloud-run needs --project <id> (or {PROJECT_ENV}): the Google Cloud project to deploy into"
            ))
        })?;
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let auth = GcpAuth::from_local_env()
        .map_err(|error| Failure::new("missing_credentials", error.message))?
        .ok_or_else(|| {
            Failure::new(
                "missing_credentials",
                format!(
                    "no Google credentials: sign in with `gcloud auth application-default login`, set \
                     GOOGLE_APPLICATION_CREDENTIALS to a service-account key or credentials file, or pass an access \
                     token in {ACCESS_TOKEN_ENV} (docs/deploy-cloud-run.md)"
                ),
            )
        })?;
    let service = match app {
        Some(app) => app.to_owned(),
        None => service_name_for(deployment)?,
    };
    let mut settings = DirectSettings::new(project, region.unwrap_or(DEFAULT_REGION));
    settings.labels = labels;
    if let Some(api_url) = std::env::var(API_URL_ENV).ok().filter(|url| !url.is_empty()) {
        settings.endpoints = Endpoints::all_at(&api_url);
        settings.poll_interval = std::time::Duration::from_millis(20);
        settings.retry_base = std::time::Duration::from_millis(10);
    }
    Ok(CloudRunDirect::new(settings, &auth, service)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn labels_are_key_value_pairs() {
        let labels = parse_labels(&["graphrt-test=1".to_owned(), "team=".to_owned()]).unwrap();
        assert_eq!(labels.get("graphrt-test").map(String::as_str), Some("1"));
        assert!(parse_labels(&["nope".to_owned()]).is_err());
    }
}
