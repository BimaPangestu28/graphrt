//! `graphrt deploy --target fly --direct` (feature `deploy-fly`): the Fly
//! side of [`super`], see `docs/deploy-direct.md`.

use graphrt_provider_api::direct::StepEvent;
use graphrt_provider_fly::direct::{DeployRequest, DirectError, DirectSettings, FlyDirect, app_name_for};
use graphrt_provider_fly::{FLY_API_TOKEN_ENV, FlyToken, TokenOrigin};
use serde_json::{Value, json};

use super::{Failure, Reporter, common_deploy, runtime};
use crate::deploy_cmd::{DeployAction, DeployRunArgs, DirectTargetArgs};

/// Image repository of the published graphrt image (agent flavour).
const IMAGE_REPOSITORY: &str = "ghcr.io/graphrt/graphrt";
/// Overrides the Machines API URL (tests, or the internal `http://_api.internal:4280`).
const API_URL_ENV: &str = "GRAPHRT_FLY_API_URL";
/// Organization a missing app is created in without `--org`.
const DEFAULT_ORG: &str = "personal";

impl From<DirectError> for Failure {
    fn from(error: DirectError) -> Self {
        Self::new(error.code.as_str(), error.message)
    }
}

/// `graphrt deploy --target fly --direct ... <agent>`.
pub(super) fn run_deploy(args: &DeployRunArgs, reporter: &mut Reporter) -> Result<(), Failure> {
    if args.project.is_some() || !args.labels.is_empty() {
        return Err(Failure::invalid("--project and --label are Cloud Run options (--target cloud-run)"));
    }
    let common = common_deploy(args, reporter)?;
    let direct = direct_for(&common.deployment, args.app.as_deref())?;
    let request = DeployRequest {
        deployment: common.deployment,
        version: common.version,
        region: args.region.clone().unwrap_or_default(),
        mode: common.mode,
        org_slug: args.org.clone().unwrap_or_else(|| DEFAULT_ORG.to_owned()),
        image: args.image.clone().unwrap_or_else(default_image),
        secrets: common.secrets,
        api_key_candidate: common.api_key_candidate,
        rotate_api_key: args.rotate_api_key,
        agent: common.agent,
        now_ms: graphrt_core::store::now_ms(),
        serve: common.serve,
        drop_store: args.serve.drop_store,
    };
    let outcome = runtime()?
        .block_on(direct.deploy(&request, &mut |event| reporter.step(&event)))
        .map_err(|failure| Failure { api_key: failure.api_key, ..Failure::from(failure.error) })?;
    let fields = json!({
        "action": "deploy", "url": outcome.url, "version": outcome.version, "app": outcome.app,
        "workflow": outcome.workflow, "serve": outcome.serve, "store": outcome.store,
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
        let note = if store.in_use { "in use" } else { "kept, not used by this version: --drop-store deletes it" };
        human.push_str(&format!(
            "\nstore: Fly volume {} ({} GB in {}, billed by Fly; {note})",
            store.volume, store.size_gb, store.region
        ));
    }
    reporter.result(fields, outcome.api_key.as_deref(), &human);
    Ok(())
}

/// `graphrt deploy status|list-versions|rollback|pause|resume|remove --target fly --direct`.
pub(super) fn run_action(action: &DeployAction, reporter: &Reporter) -> Result<(), Failure> {
    let target: &DirectTargetArgs = action.target();
    if target.project.is_some() || target.region.is_some() {
        return Err(Failure::invalid("--project and --region are Cloud Run options here (--target cloud-run)"));
    }
    let direct = direct_for(&target.deployment, target.app.as_deref())?;
    let runtime = runtime()?;
    let mut progress = |event: StepEvent| reporter.step(&event);
    let (fields, human) = match action {
        DeployAction::Status(_) => {
            let status = runtime.block_on(direct.status(graphrt_core::store::now_ms()))?;
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
            let versions = runtime.block_on(direct.list_versions())?;
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
                "app": direct.app_name(), "url": direct.url(), "current_version": current_version, "versions": versions,
            });
            (fields, human)
        }
        DeployAction::Rollback { version, .. } => {
            let serving = runtime.block_on(direct.rollback(version, &mut progress))?;
            let human = format!("{} now serves {}", serving.url, serving.version);
            (serde_json::to_value(&serving).unwrap_or(Value::Null), human)
        }
        DeployAction::Pause(_) => {
            let changed = runtime.block_on(direct.pause())?;
            let human = format!("{} paused{}", direct.app_name(), if changed { "" } else { " (already)" });
            (json!({"app": direct.app_name(), "url": direct.url(), "status": "paused", "changed": changed}), human)
        }
        DeployAction::Resume(_) => {
            let serving = runtime.block_on(direct.resume(&mut progress))?;
            let version = serving.as_ref().map(|serving| serving.version.clone());
            let human = format!("{} online{}", direct.url(), if serving.is_some() { "" } else { " (was not paused)" });
            let fields = json!({
                "app": direct.app_name(), "url": direct.url(), "status": "online",
                "version": version, "changed": serving.is_some(),
            });
            (fields, human)
        }
        DeployAction::Remove(_) => {
            let removed = runtime.block_on(direct.remove())?;
            let human = format!("{} {}", direct.app_name(), if removed { "removed" } else { "did not exist" });
            (json!({"app": direct.app_name(), "removed": removed}), human)
        }
    };
    let mut fields = fields;
    if let Some(object) = fields.as_object_mut() {
        object.insert("action".to_owned(), Value::from(action.name()));
    }
    reporter.result(fields, None, &human);
    Ok(())
}

/// The deployer of a deployment's app, with the token from `FLY_API_TOKEN`.
fn direct_for(deployment: &str, app: Option<&str>) -> Result<FlyDirect, Failure> {
    if FlyToken::from_env().is_none() {
        return Err(Failure::new(
            "missing_token",
            format!(
                "set {FLY_API_TOKEN_ENV} to a Fly token that may create apps and set secrets (docs/deploy-direct.md)"
            ),
        ));
    }
    let app_name = match app {
        Some(app) => app.to_owned(),
        None => app_name_for(deployment)?,
    };
    let mut settings = DirectSettings::new(TokenOrigin::Env(FLY_API_TOKEN_ENV.to_owned()));
    if let Some(api_url) = std::env::var(API_URL_ENV).ok().filter(|url| !url.is_empty()) {
        settings.api_base_url = api_url;
    }
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    Ok(FlyDirect::new(settings, app_name)?)
}

/// The published image of this graphrt version.
fn default_image() -> String {
    format!("{IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_image_is_the_published_one_of_this_version() {
        assert_eq!(default_image(), format!("ghcr.io/graphrt/graphrt:{}", env!("CARGO_PKG_VERSION")));
    }
}
