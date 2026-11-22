//! The few Kubernetes object fields the provider reads, as serde types.
//! Unknown fields are ignored, so new API versions do not break decoding.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;

/// Annotation the ReplicaSet controller orders scale-in victims by (lowest first).
pub const POD_DELETION_COST: &str = "controller.kubernetes.io/pod-deletion-cost";
/// Label tying an EndpointSlice to its Service.
pub const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
/// Labels the deploy driver puts on every object of a deployment version.
pub const DEPLOYMENT_LABEL: &str = "graphrt.dev/deployment";
pub const VERSION_LABEL: &str = "graphrt.dev/version";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub resource_version: String,
    pub deletion_timestamp: Option<String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMeta {
    #[serde(default)]
    pub resource_version: String,
}

/// `{kind}List`.
#[derive(Debug, Clone, Deserialize)]
pub struct ObjectList<T> {
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
}

/// `autoscaling/v1` `Scale` subresource of a Deployment.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Scale {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: ScaleSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScaleSpec {
    #[serde(default)]
    pub replicas: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pod {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: PodSpec,
    #[serde(default)]
    pub status: PodStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PodSpec {
    #[serde(default)]
    pub containers: Vec<Container>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Container {
    #[serde(default)]
    pub ports: Vec<ContainerPort>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    pub container_port: u16,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    pub phase: Option<String>,
    #[serde(rename = "podIP")]
    pub pod_ip: Option<String>,
    #[serde(default)]
    pub conditions: Vec<PodCondition>,
    #[serde(default)]
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PodCondition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerStatus {
    #[serde(default)]
    pub state: ContainerState,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerState {
    pub waiting: Option<ContainerStateWaiting>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerStateWaiting {
    pub reason: Option<String>,
    pub message: Option<String>,
}

impl Pod {
    /// The pod's `Ready` condition is `True`.
    pub fn is_ready(&self) -> bool {
        self.status.conditions.iter().any(|condition| condition.kind == "Ready" && condition.status == "True")
    }

    /// Deletion was requested (grace period running).
    pub fn is_terminating(&self) -> bool {
        self.metadata.deletion_timestamp.is_some()
    }

    /// `PodScheduled=False, reason=Unschedulable`: no node has room.
    pub fn unschedulable_message(&self) -> Option<String> {
        self.status
            .conditions
            .iter()
            .find(|condition| {
                condition.kind == "PodScheduled"
                    && condition.status == "False"
                    && condition.reason.as_deref() == Some("Unschedulable")
            })
            .map(|condition| condition.message.clone().unwrap_or_else(|| "pod unschedulable".into()))
    }

    /// A waiting reason that will not fix itself (bad image, bad config).
    pub fn fatal_waiting_reason(&self) -> Option<String> {
        const FATAL: [&str; 4] = ["InvalidImageName", "ErrImageNeverPull", "CreateContainerConfigError", "ImagePullBackOff"];
        self.status.container_statuses.iter().filter_map(|status| status.state.waiting.as_ref()).find_map(|waiting| {
            let reason = waiting.reason.as_deref()?;
            FATAL.contains(&reason).then(|| format!("{reason}: {}", waiting.message.clone().unwrap_or_default()))
        })
    }

    /// `podIP:port`, with the port named `http` or the first declared one.
    pub fn address(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.status.pod_ip.as_deref()?.parse().ok()?;
        let ports: Vec<&ContainerPort> = self.spec.containers.iter().flat_map(|container| &container.ports).collect();
        let port = ports.iter().find(|port| port.name.as_deref() == Some("http")).or(ports.first())?;
        Some(SocketAddr::new(ip, port.container_port))
    }
}

/// `discovery.k8s.io/v1` `EndpointSlice`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EndpointSlice {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub endpoints: Vec<SliceEndpoint>,
    #[serde(default)]
    pub ports: Option<Vec<SlicePort>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceEndpoint {
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub conditions: EndpointConditions,
    pub target_ref: Option<ObjectReference>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EndpointConditions {
    pub ready: Option<bool>,
    pub terminating: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObjectReference {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlicePort {
    pub name: Option<String>,
    pub port: Option<u16>,
}

/// A ready pod address found in an EndpointSlice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadyAddress {
    pub pod: String,
    pub address: SocketAddr,
}

impl EndpointSlice {
    /// Ready (and not terminating) pod endpoints of this slice.
    pub fn ready_addresses(&self) -> Vec<ReadyAddress> {
        let ports = self.ports.as_deref().unwrap_or_default();
        let Some(port) = ports.iter().find(|port| port.name.as_deref() == Some("http")).or(ports.first()).and_then(|port| port.port)
        else {
            return Vec::new();
        };
        self.endpoints
            .iter()
            .filter(|endpoint| endpoint.conditions.ready != Some(false) && endpoint.conditions.terminating != Some(true))
            .filter_map(|endpoint| {
                let pod = endpoint.target_ref.as_ref().filter(|target| target.kind == "Pod")?.name.clone();
                let ip: IpAddr = endpoint.addresses.first()?.parse().ok()?;
                Some(ReadyAddress { pod, address: SocketAddr::new(ip, port) })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_addresses_skip_unready_terminating_and_foreign_targets() {
        let slice: EndpointSlice = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "svc-abc"},
            "ports": [{"name": "metrics", "port": 9090}, {"name": "http", "port": 8080}],
            "endpoints": [
                {"addresses": ["10.0.0.1"], "conditions": {"ready": true}, "targetRef": {"kind": "Pod", "name": "p1"}},
                {"addresses": ["10.0.0.2"], "conditions": {"ready": false}, "targetRef": {"kind": "Pod", "name": "p2"}},
                {"addresses": ["10.0.0.3"], "conditions": {"ready": true, "terminating": true}, "targetRef": {"kind": "Pod", "name": "p3"}},
                {"addresses": ["10.0.0.4"], "conditions": {}, "targetRef": {"kind": "Node", "name": "n"}},
                {"addresses": ["fd00::5"], "conditions": {}, "targetRef": {"kind": "Pod", "name": "p5"}}
            ]
        }))
        .unwrap();
        let ready = slice.ready_addresses();
        assert_eq!(
            ready,
            vec![
                ReadyAddress { pod: "p1".into(), address: "10.0.0.1:8080".parse().unwrap() },
                ReadyAddress { pod: "p5".into(), address: "[fd00::5]:8080".parse().unwrap() },
            ]
        );
    }

    #[test]
    fn pod_status_helpers() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "p"},
            "spec": {"containers": [{"ports": [{"containerPort": 8080, "name": "http"}]}]},
            "status": {
                "podIP": "10.1.2.3",
                "conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/3 nodes are available"}],
                "containerStatuses": [{"state": {"waiting": {"reason": "ImagePullBackOff", "message": "not found"}}}]
            }
        }))
        .unwrap();
        assert!(!pod.is_ready() && !pod.is_terminating());
        assert_eq!(pod.unschedulable_message().as_deref(), Some("0/3 nodes are available"));
        assert_eq!(pod.fatal_waiting_reason().as_deref(), Some("ImagePullBackOff: not found"));
        assert_eq!(pod.address(), Some("10.1.2.3:8080".parse().unwrap()));
    }
}
