//! The Cloud Run `Service` of a direct deployment: the body a deploy sends,
//! the traffic of each phase (new revision without traffic, then all of it),
//! and what status reads back.

use graphrt_provider_api::direct::{STORE_PATH, ServeOptions, ServiceMode};
use serde_json::{Value, json};

use super::naming::{label_value, tag_for};
use super::resources::{AGENTS_PREFIX, DeploymentSecret, STORE_PREFIX};
use super::{DeployRequest, DirectError, TrafficSummary};

/// Port `graphrt serve` listens on (Cloud Run's `$PORT`).
pub(super) const CONTAINER_PORT: u16 = 8080;
/// Where the agent's folder of the bucket is mounted.
pub(super) const WORKFLOW_DIR: &str = "/workflows";
/// Cloud Run sends SIGKILL 10 s after SIGTERM: drain within 8.
const DRAIN_TIMEOUT: &str = "8s";
/// Requests one instance takes at once (each run is one streamed request).
const CONCURRENCY: u32 = 80;
/// Cloud Run's longest request (one SSE stream).
const REQUEST_TIMEOUT: &str = "3600s";
/// Annotation of a revision with its version label as given.
pub(super) const VERSION_ANNOTATION: &str = "graphrt.dev/version";
/// Revision label: the version's traffic tag.
pub(super) const VERSION_LABEL: &str = "graphrt-version";
/// Revision label: the workflow name.
pub(super) const WORKFLOW_LABEL: &str = "graphrt-workflow";
/// Revision label: `always-on` or `scale-to-zero`.
pub(super) const MODE_LABEL: &str = "graphrt-mode";
/// Revision label: `1` when the revision mounts the store.
pub(super) const STORE_LABEL: &str = "graphrt-store";
/// Revision label: the agent's folder in the bucket.
pub(super) const AGENT_LABEL: &str = "graphrt-agent";
/// Service label: `1` while paused.
pub(super) const PAUSED_LABEL: &str = "graphrt-paused";

/// What a version's revision is made of.
#[derive(Debug)]
pub(super) struct RevisionPlan<'a> {
    pub(super) request: &'a DeployRequest,
    pub(super) workflow: &'a str,
    pub(super) image: &'a str,
    pub(super) bucket: &'a str,
    /// The agent's folder name under `agents/` (16 hex digits of its SHA-256).
    pub(super) agent_dir: &'a str,
    pub(super) service_account: &'a str,
    /// Secrets the revision reads, the API key's among them.
    pub(super) secrets: &'a [DeploymentSecret],
    /// The service's public URL when it is known (for pairing links).
    pub(super) public_url: &'a str,
    pub(super) labels: Value,
}

/// Memory of an instance: enough to unpack the agent (512 MiB is Cloud Run's floor with gen2).
fn memory_for(unpacked_bytes: u64) -> &'static str {
    const MIB: u64 = 1024 * 1024;
    match unpacked_bytes {
        bytes if bytes > 600 * MIB => "4Gi",
        bytes if bytes > 250 * MIB => "2Gi",
        bytes if bytes > 80 * MIB => "1Gi",
        _ => "512Mi",
    }
}

/// The `graphrt serve` command line of a revision.
pub(super) fn serve_args(serve: &ServeOptions, workflow: &str, public_url: &str) -> Vec<String> {
    let mut args: Vec<String> = ["serve", WORKFLOW_DIR, "--addr"].into_iter().map(str::to_owned).collect();
    args.extend([format!("0.0.0.0:{CONTAINER_PORT}"), "--drain-timeout".to_owned(), DRAIN_TIMEOUT.to_owned()]);
    args.extend(serve.serve_flags(workflow, public_url, serve.needs_store()));
    args
}

/// The template of a new revision.
pub(super) fn render_template(plan: &RevisionPlan<'_>) -> Value {
    let request = plan.request;
    let with_store = request.serve.needs_store();
    let always_on = request.mode == ServiceMode::AlwaysOn;
    let mut env = vec![
        json!({ "name": "GRAPHRT_DEPLOYMENT", "value": request.deployment }),
        json!({ "name": "GRAPHRT_DEPLOYMENT_VERSION", "value": request.version }),
    ];
    for secret in plan.secrets {
        env.push(json!({
            "name": secret.env,
            "valueSource": { "secretKeyRef": { "secret": secret.id, "version": "latest" } },
        }));
    }
    let mut volumes = vec![json!({
        "name": "agent",
        "gcs": { "bucket": plan.bucket, "readOnly": true, "mountOptions": [format!("only-dir={AGENTS_PREFIX}{}", plan.agent_dir)] },
    })];
    let mut mounts = vec![json!({ "name": "agent", "mountPath": WORKFLOW_DIR })];
    if with_store {
        volumes.push(json!({
            "name": "store",
            "gcs": { "bucket": plan.bucket, "readOnly": false, "mountOptions": [format!("only-dir={}", STORE_PREFIX.trim_end_matches('/'))] },
        }));
        mounts.push(json!({ "name": "store", "mountPath": STORE_PATH }));
    }
    let mut labels = plan.labels.clone();
    if let Some(object) = labels.as_object_mut() {
        object.insert(VERSION_LABEL.to_owned(), Value::from(tag_for(&request.version)));
        object.insert(WORKFLOW_LABEL.to_owned(), Value::from(label_value(plan.workflow)));
        object.insert(MODE_LABEL.to_owned(), Value::from(request.mode.as_str()));
        object.insert(STORE_LABEL.to_owned(), Value::from(if with_store { "1" } else { "0" }));
        object.insert(AGENT_LABEL.to_owned(), Value::from(plan.agent_dir));
    }
    json!({
        "labels": labels,
        "annotations": { VERSION_ANNOTATION: request.version },
        "serviceAccount": plan.service_account,
        "executionEnvironment": "EXECUTION_ENVIRONMENT_GEN2",
        // One instance with a store: Cloud Storage FUSE has no locks across instances.
        "scaling": { "minInstanceCount": u32::from(always_on), "maxInstanceCount": 1 },
        "maxInstanceRequestConcurrency": CONCURRENCY,
        "timeout": REQUEST_TIMEOUT,
        "volumes": volumes,
        "containers": [{
            "name": "graphrt",
            "image": plan.image,
            "args": serve_args(&request.serve, plan.workflow, plan.public_url),
            "ports": [{ "name": "http1", "containerPort": CONTAINER_PORT }],
            "env": env,
            "resources": {
                "limits": { "cpu": "1", "memory": memory_for(request.agent.unpacked_bytes) },
                // Always on: the CPU stays on between requests (Telegram polls, Slack's socket).
                "cpuIdle": !always_on,
                "startupCpuBoost": true,
            },
            "startupProbe": {
                "httpGet": { "path": "/readyz", "port": CONTAINER_PORT },
                "initialDelaySeconds": 0, "periodSeconds": 2, "timeoutSeconds": 2, "failureThreshold": 90,
            },
            "volumeMounts": mounts,
        }],
    })
}

/// The whole service of a deploy: public, the new template, `traffic` as given.
pub(super) fn render_service(template: Value, labels: Value, traffic: Value) -> Value {
    json!({
        "labels": labels,
        "ingress": "INGRESS_TRAFFIC_ALL",
        "invokerIamDisabled": true,
        "scaling": { "scalingMode": "AUTOMATIC" },
        "template": template,
        "traffic": traffic,
    })
}

/// One traffic target of an existing service, by revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) revision: String,
    pub(super) percent: u32,
    pub(super) tag: Option<String>,
}

/// The traffic as it is, every target pinned to its revision (`LATEST`
/// resolved to the latest ready revision).
pub(super) fn current_targets(service: &Value) -> Vec<Target> {
    let latest_ready = service.get("latestReadyRevision").and_then(Value::as_str).map(short_name);
    let mut targets = Vec::new();
    for target in service.get("traffic").and_then(Value::as_array).into_iter().flatten() {
        let revision = match target.get("type").and_then(Value::as_str) {
            Some("TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST") => latest_ready.clone(),
            _ => target.get("revision").and_then(Value::as_str).map(short_name),
        };
        let Some(revision) = revision else { continue };
        let percent = target.get("percent").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()).unwrap_or(0);
        let tag = target.get("tag").and_then(Value::as_str).filter(|tag| !tag.is_empty()).map(str::to_owned);
        targets.push(Target { revision, percent, tag });
    }
    targets
}

fn render_targets(targets: &[Target]) -> Value {
    let mut rendered = Vec::new();
    for target in targets {
        let mut entry = json!({
            "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION",
            "revision": target.revision,
            "percent": target.percent,
        });
        if let (Some(tag), Some(object)) = (&target.tag, entry.as_object_mut()) {
            object.insert("tag".to_owned(), Value::from(tag.as_str()));
        }
        rendered.push(entry);
    }
    Value::Array(rendered)
}

/// Traffic while the new revision starts: everything stays where it is, and
/// the new revision gets `tag` with 0 % (a first deploy: 100 %, nothing else serves).
pub(super) fn candidate_traffic(existing: &[Target], tag: &str) -> Value {
    let serving = existing.iter().any(|target| target.percent > 0);
    let latest = json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": if serving { 0 } else { 100 }, "tag": tag });
    let kept: Vec<Target> = existing
        .iter()
        .filter(|target| target.percent > 0 || target.tag.as_deref() != Some(tag))
        .map(|target| Target { tag: target.tag.clone().filter(|existing_tag| existing_tag != tag), ..target.clone() })
        .collect();
    let mut traffic = vec![latest];
    if let Value::Array(rest) = render_targets(&merge_untagged(kept)) {
        traffic.extend(rest);
    }
    Value::Array(traffic)
}

/// Traffic once `revision` is ready: all of it to `revision` under `tag`, the
/// other tags kept at 0 % (newest first, at most `keep` tags in all).
pub(super) fn promoted_traffic(existing: &[Target], revision: &str, tag: &str, keep: usize) -> (Value, Vec<String>) {
    let mut targets = vec![Target { revision: revision.to_owned(), percent: 100, tag: Some(tag.to_owned()) }];
    let mut dropped = Vec::new();
    for target in existing {
        let Some(existing_tag) = &target.tag else { continue };
        if existing_tag == tag || target.revision == revision {
            continue;
        }
        if targets.len() < keep.max(1) {
            targets.push(Target { percent: 0, ..target.clone() });
        } else {
            dropped.push(existing_tag.clone());
        }
    }
    (render_targets(&targets), dropped)
}

/// Traffic without `tag` (a candidate that never got ready).
pub(super) fn without_tag(existing: &[Target], tag: &str) -> Value {
    let kept: Vec<Target> = existing
        .iter()
        .filter(|target| target.percent > 0 || target.tag.as_deref() != Some(tag))
        .map(|target| Target { tag: target.tag.clone().filter(|existing_tag| existing_tag != tag), ..target.clone() })
        .collect();
    render_targets(&merge_untagged(kept))
}

/// Untagged targets of one revision become one (Cloud Run refuses duplicates).
fn merge_untagged(targets: Vec<Target>) -> Vec<Target> {
    let mut merged: Vec<Target> = Vec::new();
    for target in targets {
        if target.tag.is_none()
            && let Some(existing) =
                merged.iter_mut().find(|other| other.tag.is_none() && other.revision == target.revision)
        {
            existing.percent += target.percent;
            continue;
        }
        if target.tag.is_none() && target.percent == 0 {
            continue;
        }
        merged.push(target);
    }
    merged
}

/// The traffic as status reports it, with version labels from the revisions.
pub(super) fn traffic_summary(targets: &[Target], version_of: impl Fn(&str) -> Option<String>) -> Vec<TrafficSummary> {
    targets
        .iter()
        .map(|target| TrafficSummary {
            version: version_of(&target.revision),
            revision: target.revision.clone(),
            percent: target.percent,
        })
        .collect()
}

/// `projects/p/locations/r/services/s/revisions/x` -> `x`.
pub(super) fn short_name(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_owned()
}

/// Whether the service was created by a direct deploy.
pub(super) fn is_direct(service: &Value) -> bool {
    service.pointer("/labels/graphrt-direct").and_then(Value::as_str) == Some("1")
}

/// Refuses a service of the same name that a direct deploy did not create.
pub(super) fn check_ours(service: &Value, name: &str) -> Result<(), DirectError> {
    if is_direct(service) {
        return Ok(());
    }
    Err(DirectError::new(
        super::ErrorCode::NameTaken,
        format!("the Cloud Run service {name} exists and was not made by graphrt deploy --direct: pass another --app"),
    ))
}

/// Cloud Run's message about the last change, when it failed.
pub(super) fn failure_message(service: &Value) -> Option<String> {
    let condition = service.get("terminalCondition")?;
    let failed = condition.get("state").and_then(Value::as_str) == Some("CONDITION_FAILED");
    failed.then(|| condition.get("message").and_then(Value::as_str).unwrap_or("the last change failed").to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use graphrt_provider_api::direct::{AgentFile, ChatOptions};

    use super::super::API_KEY_ENV;

    use super::*;

    fn request(serve: ServeOptions, mode: ServiceMode) -> DeployRequest {
        DeployRequest {
            deployment: "Support Bot".into(),
            version: "v1".into(),
            mode,
            image: None,
            secrets: BTreeMap::new(),
            api_key_candidate: "grk_x".into(),
            rotate_api_key: false,
            agent: AgentFile::new("support.yaml", b"x".to_vec()),
            serve,
            drop_store: false,
        }
    }

    fn plan<'a>(request: &'a DeployRequest, secrets: &'a [DeploymentSecret]) -> RevisionPlan<'a> {
        RevisionPlan {
            request,
            workflow: "support",
            image: "us-central1-docker.pkg.dev/p/graphrt-ghcr/graphrt/graphrt:0.6.3",
            bucket: "p-support-bot",
            agent_dir: "0123456789abcdef",
            service_account: "graphrt-support-bot@p.iam.gserviceaccount.com",
            secrets,
            public_url: "https://support-bot-1.us-central1.run.app",
            labels: json!({ "managed-by": "graphrt" }),
        }
    }

    #[test]
    fn a_revision_mounts_its_agent_and_reads_secrets_by_reference() {
        let secrets = [DeploymentSecret {
            name: "projects/p/secrets/support-bot-graphrt-api-key".into(),
            id: "support-bot-graphrt-api-key".into(),
            env: API_KEY_ENV.into(),
        }];
        let request = request(ServeOptions::default(), ServiceMode::ScaleToZero);
        let template = render_template(&plan(&request, &secrets));
        let container = &template["containers"][0];
        assert_eq!(
            container["args"],
            json!(["serve", "/workflows", "--addr", "0.0.0.0:8080", "--drain-timeout", "8s"])
        );
        assert_eq!(container["env"][2]["valueSource"]["secretKeyRef"]["secret"], "support-bot-graphrt-api-key");
        assert_eq!(container["startupProbe"]["httpGet"]["path"], "/readyz");
        assert_eq!(template["volumes"][0]["gcs"]["mountOptions"][0], "only-dir=agents/0123456789abcdef");
        assert_eq!(template["volumes"].as_array().map(Vec::len), Some(1), "no store without a feature that needs it");
        assert_eq!(template["scaling"], json!({ "minInstanceCount": 0, "maxInstanceCount": 1 }));
        assert_eq!(container["resources"]["cpuIdle"], true);
        assert_eq!(template["annotations"][VERSION_ANNOTATION], "v1");
        assert_eq!(template["labels"][VERSION_LABEL], "ver-v1");
    }

    #[test]
    fn the_store_is_a_writable_folder_of_the_bucket() {
        let serve = ServeOptions { conversations: true, chat: Some(ChatOptions::default()), ..ServeOptions::default() };
        let request = request(serve, ServiceMode::AlwaysOn);
        let template = render_template(&plan(&request, &[]));
        assert_eq!(
            template["volumes"][1]["gcs"],
            json!({ "bucket": "p-support-bot", "readOnly": false, "mountOptions": ["only-dir=store"] })
        );
        assert_eq!(template["containers"][0]["volumeMounts"][1]["mountPath"], "/data");
        let args = template["containers"][0]["args"].as_array().cloned().unwrap_or_default();
        assert!(args.contains(&json!("--store")) && args.contains(&json!("--conversations")), "{args:?}");
        assert_eq!(template["scaling"]["minInstanceCount"], 1);
        assert_eq!(template["containers"][0]["resources"]["cpuIdle"], false);
        assert_eq!(template["labels"][STORE_LABEL], "1");
    }

    #[test]
    fn a_candidate_gets_no_traffic_until_it_is_promoted() {
        assert_eq!(
            candidate_traffic(&[], "v1"),
            json!([{ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": "v1" }])
        );
        let existing = vec![
            Target { revision: "s-00002".into(), percent: 100, tag: Some("v2".into()) },
            Target { revision: "s-00001".into(), percent: 0, tag: Some("v1".into()) },
        ];
        let traffic = candidate_traffic(&existing, "v3");
        assert_eq!(traffic[0], json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 0, "tag": "v3" }));
        assert_eq!(traffic[1]["percent"], 100);
        // Redeploying the serving label: it keeps its traffic, the tag moves to the candidate.
        let again = candidate_traffic(&existing, "v2");
        assert_eq!(
            again[1],
            json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-00002", "percent": 100 })
        );
        let (promoted, dropped) = promoted_traffic(&existing, "s-00003", "v3", 2);
        assert_eq!(promoted[0]["percent"], 100);
        assert_eq!(
            promoted[1],
            json!({ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-00002", "percent": 0, "tag": "v2" })
        );
        assert_eq!(dropped, ["v1"]);
        let failed = without_tag(&existing, "v1");
        assert_eq!(failed.as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn latest_targets_resolve_to_the_ready_revision() {
        let service = json!({
            "latestReadyRevision": "projects/p/locations/r/services/s/revisions/s-00004",
            "traffic": [
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": "v4" },
                { "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "s-00003", "percent": 0, "tag": "v3" },
            ],
        });
        let targets = current_targets(&service);
        assert_eq!(targets[0], Target { revision: "s-00004".into(), percent: 100, tag: Some("v4".into()) });
        assert_eq!(targets[1].revision, "s-00003");
    }
}
