//! Revision listing and retirement of old deployment versions, shared by the
//! run-time provider and the deploy driver.

use serde_json::{Value, json};

use super::revisions::{Revision, plan_retirement, traffic_labels};
use super::{AcaConfig, checked_app_name};
use crate::arm::ArmClient;
use crate::http::json_at;
use graphrt_provider_api::{ProviderError, ProviderErrorKind};

/// Revisions of `app` as ARM reports them.
pub(crate) async fn list_revisions(
    arm: &ArmClient,
    config: &AcaConfig,
    app: &str,
) -> Result<Vec<Revision>, ProviderError> {
    let app = checked_app_name(app)?;
    let response = arm.get(&config.app_path(app, "/revisions")).await?;
    Ok(json_at(&response.body, "/value")
        .as_array()
        .map(|entries| entries.iter().filter_map(|entry| Revision::from_arm(app, entry)).collect())
        .unwrap_or_default())
}

/// Retire versions whose successor is older than the retention window, and
/// active revisions that carry no version label and are not the latest (left
/// behind by a failed deploy or a manual change): drop their traffic labels,
/// then deactivate them. Returns the deactivated revision names.
pub(crate) async fn retire_old_versions(
    arm: &ArmClient,
    config: &AcaConfig,
    app: &str,
    now_unix: u64,
) -> Result<Vec<String>, ProviderError> {
    let app = checked_app_name(app)?;
    let current = arm.get(&config.app_path(app, "")).await?.body;
    let revisions = list_revisions(arm, config, app).await?;
    let retired = revisions_to_retire(&current, &revisions, now_unix, config);
    if retired.is_empty() {
        return Ok(retired);
    }
    let kept_traffic: Vec<Value> = json_at(&current, "/properties/configuration/ingress/traffic")
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| {
                    json_at(entry, "/revisionName").as_str().is_none_or(|name| !retired.iter().any(|r| r == name))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let patch = json!({ "properties": { "configuration": { "ingress": { "traffic": kept_traffic } } } });
    let started = arm.patch(&config.app_path(app, ""), &patch).await?;
    arm.wait(&started).await?;
    for revision in &retired {
        match arm.post(&config.app_path(app, &format!("/revisions/{revision}/deactivate"))).await {
            Ok(_) => {}
            Err(error) if error.kind == ProviderErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(retired)
}

fn revisions_to_retire(app: &Value, revisions: &[Revision], now_unix: u64, config: &AcaConfig) -> Vec<String> {
    let labelled = traffic_labels(app);
    let latest = json_at(app, "/properties/latestRevisionName").as_str().unwrap_or("");
    let mut retired = plan_retirement(revisions, now_unix, config.old_version_retention);
    retired.extend(
        revisions
            .iter()
            .filter(|revision| revision.active && revision.name != latest)
            .filter(|revision| revision.label.as_ref().is_none_or(|label| !labelled.contains(label)))
            .map(|revision| revision.name.clone()),
    );
    retired.sort();
    retired.dedup();
    retired
}
