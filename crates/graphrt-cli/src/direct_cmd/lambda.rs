//! `graphrt deploy --target lambda --direct` (feature `deploy-aws`): the AWS
//! Lambda side of [`super`], see `docs/deploy-direct-aws.md`. Same flags, JSON
//! Lines and exit codes as Fly and Cloud Run, plus `--profile`.
//!
//! Credentials come from the standard AWS places, in this order: the profile
//! named by `--profile` or `AWS_PROFILE` (`~/.aws/credentials`, `~/.aws/config`);
//! else `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`; else
//! the `default` profile; else a container or instance role. The region comes from
//! `--region`, else `AWS_REGION` / `AWS_DEFAULT_REGION`, else the profile.

use std::sync::Arc;

use graphrt_provider_api::direct::{StepEvent, workflow_name_of};
use graphrt_provider_aws::auth::profile::{SharedFiles, load_profile};
use graphrt_provider_aws::auth::{ChainConfig, Clock, CredentialChain};
use graphrt_provider_aws::direct::{
    DEFAULT_ADAPTER_IMAGE, DeployRequest, DirectError, DirectSettings, ErrorCode, LambdaDirect, function_name_for,
    is_region_code,
};
use serde_json::{Value, json};

use super::{Failure, Reporter, common_deploy, runtime};
use crate::deploy_cmd::{DeployAction, DeployRunArgs, DirectMode, DirectTargetArgs};

/// Overrides every AWS endpoint (tests against a fake AWS).
const ENDPOINT_ENV: &str = "GRAPHRT_AWS_ENDPOINT_URL";
/// Overrides the Lambda Web Adapter image.
const ADAPTER_IMAGE_ENV: &str = "GRAPHRT_LAMBDA_ADAPTER_IMAGE";

/// Image repository of the published graphrt image (agent flavour).
const IMAGE_REPOSITORY: &str = "ghcr.io/graphrt/graphrt";

impl From<DirectError> for Failure {
    fn from(error: DirectError) -> Self {
        Self::new(error.code.as_str(), error.message)
    }
}

/// Flags of the Fly deployer that mean nothing on Lambda.
fn refuse_fly_only_flags(args: &DeployRunArgs) -> Result<(), Failure> {
    if args.mode == Some(DirectMode::AlwaysOn) {
        return Err(Failure::invalid(
            "--mode always-on is not offered on Lambda: an instance starts on each request and nothing runs (or is \
             billed) while idle. Use --target fly --mode always-on for an agent that must keep running",
        ));
    }
    if args.org.is_some() {
        return Err(Failure::invalid("--org is a Fly setting: on AWS the account comes from the credentials"));
    }
    if args.project.is_some() || !args.labels.is_empty() {
        return Err(Failure::invalid("--project and --label are Cloud Run options (--target cloud-run)"));
    }
    if args.serve.drop_store {
        return Err(Failure::invalid("--drop-store: a Lambda deployment has no store to drop"));
    }
    Ok(())
}

/// `role_arn` of `--set role_arn=ARN`, the only setting the Lambda deployer takes.
fn role_arn_setting(settings: &[String]) -> Result<Option<String>, Failure> {
    let mut role = None;
    for setting in settings {
        match setting.split_once('=') {
            Some(("role_arn", value)) if value.starts_with("arn:aws") && value.contains(":role/") => {
                role = Some(value.to_owned());
            }
            Some(("role_arn", _)) => {
                return Err(Failure::invalid(
                    "--set role_arn= takes an IAM role ARN (arn:aws:iam::<account>:role/<name>)",
                ));
            }
            _ => {
                return Err(Failure::invalid(format!(
                    "--set {setting}: --target lambda --direct only takes role_arn=ARN"
                )));
            }
        }
    }
    Ok(role)
}

/// `graphrt deploy --target lambda --direct ... <agent>`.
pub(super) fn run_deploy(args: &DeployRunArgs, reporter: &mut Reporter) -> Result<(), Failure> {
    refuse_fly_only_flags(args)?;
    let role_arn = role_arn_setting(&args.settings)?;
    let common = common_deploy(args, reporter)?;
    common.serve.validate().map_err(Failure::invalid)?;
    if common.serve.needs_store() {
        return Err(Failure::invalid(
            "--monitor, --devices, --push, --conversations and --channel keep state on disk (graphrt serve --store), \
             which Lambda does not have between requests. Leave them out on --target lambda, or deploy to \
             --target fly or --target cloud-run, which give the deployment a store",
        ));
    }
    let workflow = workflow_name_of(&common.agent.file_name);
    let direct = direct_for(&common.deployment, args.app.as_deref(), args.region.as_deref(), args.profile.as_deref())?;
    let request = DeployRequest {
        deployment: common.deployment,
        version: common.version,
        image: args.image.clone().unwrap_or_else(default_image),
        adapter_image: std::env::var(ADAPTER_IMAGE_ENV)
            .ok()
            .filter(|image| !image.is_empty())
            .unwrap_or_else(|| DEFAULT_ADAPTER_IMAGE.to_owned()),
        secrets: common.secrets,
        api_key_candidate: common.api_key_candidate,
        rotate_api_key: args.rotate_api_key,
        agent: common.agent,
        serve_flags: common.serve.serve_flags(&workflow, "", false),
        serve_summary: serde_json::to_value(common.serve.summary()).unwrap_or(Value::Null),
        workflow,
        role_arn,
        now_ms: graphrt_core::store::now_ms(),
    };
    let outcome = runtime()?
        .block_on(direct.deploy(&request, &mut |event| reporter.step(&event)))
        .map_err(|failure| Failure { api_key: failure.api_key, ..Failure::from(failure.error) })?;
    let fields = json!({
        "action": "deploy", "url": outcome.url, "version": outcome.version, "app": outcome.app,
        "region": outcome.region, "workflow": outcome.workflow, "serve": outcome.serve, "store": outcome.store,
    });
    let mut human = format!(
        "deployed {} to {} (POST {}/v1/workflows/{}/runs)",
        outcome.version, outcome.url, outcome.url, outcome.workflow
    );
    if outcome.serve.get("chat").is_some_and(|chat| !chat.is_null()) {
        human.push_str(&format!("\nchat page: {}/chat/", outcome.url));
    }
    if outcome.resumed {
        human.push_str("\nthe deployment was paused: this deploy put it back online");
    }
    reporter.result(fields, outcome.api_key.as_deref(), &human);
    Ok(())
}

/// `graphrt deploy status|list-versions|rollback|pause|resume|remove --target lambda --direct`.
pub(super) fn run_action(action: &DeployAction, reporter: &Reporter) -> Result<(), Failure> {
    let target: &DirectTargetArgs = action.target();
    if target.org.is_some() {
        return Err(Failure::invalid("--org is a Fly setting: on AWS the account comes from the credentials"));
    }
    if target.project.is_some() {
        return Err(Failure::invalid("--project is a Cloud Run option (--target cloud-run)"));
    }
    let direct =
        direct_for(&target.deployment, target.app.as_deref(), target.region.as_deref(), target.profile.as_deref())?;
    let runtime = runtime()?;
    let mut progress = |event: StepEvent| reporter.step(&event);
    let app = direct.function_name().to_owned();
    let (fields, human) = match action {
        DeployAction::Status(_) => {
            let status = runtime.block_on(direct.status())?;
            let human = format!(
                "{}: {} ({}), version {}, deployed {}",
                status.app,
                serde_json::to_value(status.status).unwrap_or(Value::Null).as_str().unwrap_or("unknown"),
                if status.url.is_empty() { "no URL yet" } else { status.url.as_str() },
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
                    format!("{marker} {}  {}  {}", entry.label, entry.deployed_at.as_deref().unwrap_or("-"), entry.role)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let current_version = versions.iter().find(|entry| entry.current).map(|entry| entry.label.clone());
            let url = runtime.block_on(direct.status()).map(|status| status.url).unwrap_or_default();
            let fields = json!({
                "app": app, "region": direct.region(), "url": url, "current_version": current_version, "versions": versions,
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
            let url = runtime.block_on(direct.status())?.url;
            let human = format!("{app} paused{}", if changed { "" } else { " (already)" });
            (json!({"app": app, "region": direct.region(), "url": url, "status": "paused", "changed": changed}), human)
        }
        DeployAction::Resume(_) => {
            let serving = runtime.block_on(direct.resume(&mut progress))?;
            let version = serving.as_ref().map(|serving| serving.version.clone());
            let url = match &serving {
                Some(serving) => serving.url.clone(),
                None => runtime.block_on(direct.status())?.url,
            };
            let human = format!("{app} online{}", if serving.is_some() { "" } else { " (was not paused)" });
            let fields = json!({
                "app": app, "region": direct.region(), "url": url, "status": "online",
                "version": version, "changed": serving.is_some(),
            });
            (fields, human)
        }
        DeployAction::Remove(_) => {
            let removed = runtime.block_on(direct.remove())?;
            let human = format!("{app} {}", if removed { "removed" } else { "did not exist (leftovers cleaned up)" });
            (json!({"app": app, "region": direct.region(), "removed": removed}), human)
        }
    };
    let mut fields = fields;
    if let Some(object) = fields.as_object_mut() {
        object.insert("action".to_owned(), Value::from(action.name()));
    }
    reporter.result(fields, None, &human);
    Ok(())
}

/// The published image of this graphrt version.
fn default_image() -> String {
    format!("{IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// The credential chain and the profile's region.
///
/// @throws Failure(missing_credentials) - a named profile without usable keys
fn credentials(profile_flag: Option<&str>) -> Result<(ChainConfig, Option<String>), Failure> {
    let lookup = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let mut config = ChainConfig::from_env();
    let named = profile_flag.map(str::to_owned).or_else(|| lookup("AWS_PROFILE"));
    let env_keys = config.static_credentials.is_some();
    let profile_name = match (&named, env_keys) {
        (Some(name), _) => name.clone(),
        (None, true) => return Ok((config, None)),
        (None, false) => "default".to_owned(),
    };
    let files = SharedFiles::from_lookup(lookup);
    match load_profile(&profile_name, &files) {
        Ok(profile) => {
            let region = profile.region.clone();
            match (profile.credentials, profile.delegated) {
                (Some(keys), _) => config.static_credentials = Some(keys),
                (None, Some(kind)) => {
                    return Err(Failure::new(
                        ErrorCode::MissingCredentials.as_str(),
                        format!(
                            "AWS profile {profile_name} signs in with {kind}, which graphrt does not run itself. \
                             Run `eval \"$(aws configure export-credentials --profile {profile_name} --format env)\"` \
                             and deploy again without --profile, or use a profile with an access key"
                        ),
                    ));
                }
                (None, None) if named.is_some() => {
                    return Err(Failure::new(
                        ErrorCode::MissingCredentials.as_str(),
                        format!(
                            "AWS profile {profile_name} has no access key: run `aws configure --profile {profile_name}`"
                        ),
                    ));
                }
                (None, None) => {}
            }
            Ok((config, region))
        }
        Err(error) if named.is_some() => Err(Failure::new(ErrorCode::MissingCredentials.as_str(), error.0)),
        // No profile file: a container or instance role may still provide credentials.
        Err(_) => Ok((config, None)),
    }
}

/// The deployer of a deployment's function in the chosen region.
fn direct_for(
    deployment: &str,
    function: Option<&str>,
    region_flag: Option<&str>,
    profile: Option<&str>,
) -> Result<LambdaDirect, Failure> {
    let (mut config, profile_region) = credentials(profile)?;
    let lookup = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let region = region_flag
        .map(str::to_owned)
        .or_else(|| lookup("AWS_REGION"))
        .or_else(|| lookup("AWS_DEFAULT_REGION"))
        .or(profile_region)
        .ok_or_else(|| {
            Failure::invalid("choose the AWS region with --region, for example --region ap-southeast-1 (Singapore)")
        })?;
    if !is_region_code(&region) {
        return Err(Failure::new(
            ErrorCode::RegionUnavailable.as_str(),
            format!("{region:?} is not an AWS region code: use one such as ap-southeast-1, us-east-1 or eu-west-1"),
        ));
    }
    config.region.clone_from(&region);
    let function = match function {
        Some(name) => name.to_owned(),
        None => function_name_for(deployment)?,
    };
    let mut settings = DirectSettings::new(&region);
    if let Some(endpoint) = lookup(ENDPOINT_ENV) {
        for service in
            [&mut settings.lambda, &mut settings.ecr, &mut settings.iam, &mut settings.sts, &mut settings.logs]
        {
            service.base_url.clone_from(&endpoint);
        }
        config.sts_endpoint = Some(endpoint);
        config.imds_disabled = true;
    }
    let chain = CredentialChain::new(config, Clock::default())
        .map_err(|error| Failure::new(ErrorCode::Internal.as_str(), error.to_string()))?;
    Ok(LambdaDirect::new(settings, function, Arc::new(chain))?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn only_a_role_arn_may_be_set() {
        assert_eq!(role_arn_setting(&[]).unwrap(), None);
        let role = role_arn_setting(&["role_arn=arn:aws:iam::123456789012:role/agents".to_owned()]).unwrap();
        assert_eq!(role.as_deref(), Some("arn:aws:iam::123456789012:role/agents"));
        assert!(role_arn_setting(&["role_arn=agents".to_owned()]).is_err());
        assert!(role_arn_setting(&["memory=2048".to_owned()]).unwrap_err().message.contains("only takes role_arn"));
    }
}
