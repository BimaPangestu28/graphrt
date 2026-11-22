//! What graphrt reads back from a Cloud Run `Service`: readiness, tag URLs, and
//! the cost-relevant settings checked for drift (C12).

use std::collections::BTreeMap;

use serde_json::Value;

/// A service as the Admin API reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloudRunServiceStatus {
    /// Main URL (100 % latest).
    pub uri: Option<String>,
    /// Audience of invoker ID tokens: the first custom audience, else `uri`.
    pub audience: Option<String>,
    /// `terminalCondition.state == CONDITION_SUCCEEDED`.
    pub ready: bool,
    /// Short name of the latest ready revision.
    pub latest_ready_revision: Option<String>,
    /// Tag of the `LATEST` traffic target.
    pub latest_tag: Option<String>,
    /// URL of every traffic tag (`v7` → `https://v7---svc-....a.run.app`).
    pub tag_uris: BTreeMap<String, String>,
    /// `template.scaling.minInstanceCount` (absent = 0).
    pub min_instances: u32,
    /// `template.scaling.maxInstanceCount`.
    pub max_instances: Option<u32>,
    /// `resources.cpuIdle` of the agent container (request-based billing).
    pub cpu_idle: Option<bool>,
    /// `resources.startupCpuBoost`.
    pub startup_cpu_boost: Option<bool>,
    /// `template.timeout` in seconds.
    pub timeout_secs: Option<u64>,
    /// `template.maxInstanceRequestConcurrency`.
    pub concurrency: Option<u32>,
}

impl CloudRunServiceStatus {
    /// Reads the fields graphrt cares about; missing fields stay `None`/default.
    pub fn from_service(service: &Value) -> Self {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        let number =
            |value: Option<&Value>| value.and_then(Value::as_u64).and_then(|number| u32::try_from(number).ok());
        let template = service.get("template");
        let scaling = template.and_then(|template| template.get("scaling"));
        let resources = template
            .and_then(|template| template.get("containers"))
            .and_then(Value::as_array)
            .and_then(|containers| containers.first())
            .and_then(|container| container.get("resources"));
        let uri = text(service.get("uri"));
        let audience = service
            .get("customAudiences")
            .and_then(Value::as_array)
            .and_then(|audiences| audiences.first())
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| uri.clone());
        let mut tag_uris = BTreeMap::new();
        for status in service.get("trafficStatuses").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(tag), Some(tag_uri)) = (text(status.get("tag")), text(status.get("uri"))) {
                tag_uris.insert(tag, tag_uri);
            }
        }
        let latest_tag = service
            .get("traffic")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|target| target.get("type").and_then(Value::as_str) == Some("TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST"))
            .and_then(|target| text(target.get("tag")));
        Self {
            uri,
            audience,
            ready: service.pointer("/terminalCondition/state").and_then(Value::as_str) == Some("CONDITION_SUCCEEDED"),
            latest_ready_revision: text(service.get("latestReadyRevision"))
                .map(|revision| revision.rsplit('/').next().unwrap_or_default().to_owned()),
            latest_tag,
            tag_uris,
            min_instances: number(scaling.and_then(|scaling| scaling.get("minInstanceCount"))).unwrap_or(0),
            max_instances: number(scaling.and_then(|scaling| scaling.get("maxInstanceCount"))),
            cpu_idle: resources.and_then(|resources| resources.get("cpuIdle")).and_then(Value::as_bool),
            startup_cpu_boost: resources
                .and_then(|resources| resources.get("startupCpuBoost"))
                .and_then(Value::as_bool),
            timeout_secs: template
                .and_then(|template| template.get("timeout"))
                .and_then(Value::as_str)
                .and_then(|timeout| timeout.strip_suffix('s'))
                .and_then(|seconds| seconds.split('.').next())
                .and_then(|seconds| seconds.parse().ok()),
            concurrency: number(template.and_then(|template| template.get("maxInstanceRequestConcurrency"))),
        }
    }

    /// URL of a version's tag; the main URL when the tag is the latest one but
    /// its tag URL is not reported yet.
    pub fn url_for_tag(&self, tag: &str) -> Option<&str> {
        self.tag_uris
            .get(tag)
            .map(String::as_str)
            .or_else(|| self.uri.as_deref().filter(|_| self.latest_tag.as_deref() == Some(tag)))
    }

    /// Settings changed outside graphrt that cost money or break runs, as
    /// operator-facing warnings (reconciliation, C12).
    pub fn drift_warnings(&self, expected_min_instances: u32) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.min_instances != expected_min_instances {
            warnings.push(format!(
                "minInstanceCount is {} (expected {expected_min_instances}): idle instances are billed",
                self.min_instances
            ));
        }
        if self.cpu_idle != Some(true) {
            warnings.push("cpuIdle is not true: instance-based billing, CPU billed while idle".to_owned());
        }
        if self.startup_cpu_boost != Some(true) {
            warnings.push("startupCpuBoost is off: slower cold starts".to_owned());
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_v2_service() {
        let service = serde_json::json!({
            "uri": "https://graphrt-chat-abc-ew.a.run.app",
            "customAudiences": ["graphrt/p/europe-west1/graphrt-chat"],
            "terminalCondition": { "state": "CONDITION_SUCCEEDED" },
            "latestReadyRevision": "projects/p/locations/europe-west1/services/graphrt-chat/revisions/graphrt-chat-v2",
            "traffic": [{ "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST", "percent": 100, "tag": "v2" }],
            "trafficStatuses": [{ "tag": "v2", "uri": "https://v2---graphrt-chat-abc-ew.a.run.app" }],
            "template": {
                "timeout": "3600s",
                "maxInstanceRequestConcurrency": 8,
                "scaling": { "maxInstanceCount": 3 },
                "containers": [{ "resources": { "cpuIdle": true, "startupCpuBoost": true } }],
            },
        });
        let status = CloudRunServiceStatus::from_service(&service);
        assert!(status.ready);
        assert_eq!(status.latest_ready_revision.as_deref(), Some("graphrt-chat-v2"));
        assert_eq!(status.url_for_tag("v2"), Some("https://v2---graphrt-chat-abc-ew.a.run.app"));
        assert_eq!(status.url_for_tag("v1"), None);
        assert_eq!(status.audience.as_deref(), Some("graphrt/p/europe-west1/graphrt-chat"));
        assert_eq!((status.min_instances, status.timeout_secs, status.concurrency), (0, Some(3600), Some(8)));
        assert!(status.drift_warnings(0).is_empty());
        assert_eq!(status.drift_warnings(1).len(), 1);
    }
}
