//! The few Kubernetes object fields the provider reads.
//!
//! Objects are kept as `serde_json::Value` and read through small accessors
//! instead of serde-derived structs: the JSON parser for `Value` is already in
//! the binary, while one derived struct tree per object kind cost tens of KiB
//! (plan §9 budget). Missing fields and Kubernetes' `null` for empty lists
//! read as empty; unknown fields are ignored.

use std::net::{IpAddr, SocketAddr};

use serde_json::Value;

/// Annotation the ReplicaSet controller orders scale-in victims by (lowest first).
pub const POD_DELETION_COST: &str = "controller.kubernetes.io/pod-deletion-cost";
/// Label tying an EndpointSlice to its Service.
pub const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
/// Labels the deploy driver puts on every object of a deployment version.
pub const DEPLOYMENT_LABEL: &str = "graphrt.dev/deployment";
pub const VERSION_LABEL: &str = "graphrt.dev/version";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn array<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value.pointer(pointer).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()
}

/// `metadata.name`.
pub fn name(object: &Value) -> &str {
    text(object, "/metadata/name").unwrap_or_default()
}

/// `metadata.resourceVersion` (lists and objects alike).
pub fn resource_version(object: &Value) -> &str {
    text(object, "/metadata/resourceVersion").unwrap_or_default()
}

/// `items` of a `{kind}List`.
pub fn items(list: &Value) -> &[Value] {
    array(list, "/items")
}

/// `spec.replicas` of a `Scale` (or Deployment).
pub fn spec_replicas(object: &Value) -> u32 {
    object
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .and_then(|replicas| u32::try_from(replicas).ok())
        .unwrap_or(0)
}

/// Message of the condition `kind=True` in `status.conditions`, if present.
fn true_condition<'a>(object: &'a Value, kind: &str) -> Option<&'a Value> {
    array(object, "/status/conditions")
        .iter()
        .find(|condition| text(condition, "/type") == Some(kind) && text(condition, "/status") == Some("True"))
}

/// Message of a Deployment's `ReplicaFailure=True` condition (pods the
/// ReplicaSet could not create, e.g. an exhausted ResourceQuota).
pub fn replica_failure(deployment: &Value) -> Option<String> {
    true_condition(deployment, "ReplicaFailure")
        .map(|condition| text(condition, "/message").unwrap_or("replica creation failed").to_owned())
}

/// A pod, read through accessors.
#[derive(Debug, Clone, PartialEq)]
pub struct Pod(pub Value);

impl Pod {
    /// `metadata.name`.
    pub fn name(&self) -> &str {
        name(&self.0)
    }

    /// An annotation value.
    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.0.pointer("/metadata/annotations").and_then(|annotations| annotations.get(key)).and_then(Value::as_str)
    }

    /// `status.phase` (`Pending`, `Running`, `Succeeded`, `Failed`).
    pub fn phase(&self) -> Option<&str> {
        text(&self.0, "/status/phase")
    }

    /// The pod's `Ready` condition is `True`.
    pub fn is_ready(&self) -> bool {
        true_condition(&self.0, "Ready").is_some()
    }

    /// Deletion was requested (grace period running).
    pub fn is_terminating(&self) -> bool {
        self.0.pointer("/metadata/deletionTimestamp").is_some_and(|timestamp| !timestamp.is_null())
    }

    /// `PodScheduled=False, reason=Unschedulable`: no node has room.
    pub fn unschedulable_message(&self) -> Option<String> {
        array(&self.0, "/status/conditions")
            .iter()
            .find(|condition| {
                text(condition, "/type") == Some("PodScheduled")
                    && text(condition, "/status") == Some("False")
                    && text(condition, "/reason") == Some("Unschedulable")
            })
            .map(|condition| text(condition, "/message").unwrap_or("pod unschedulable").to_owned())
    }

    /// A waiting reason that will not fix itself (bad image, bad config).
    pub fn fatal_waiting_reason(&self) -> Option<String> {
        const FATAL: [&str; 4] =
            ["InvalidImageName", "ErrImageNeverPull", "CreateContainerConfigError", "ImagePullBackOff"];
        array(&self.0, "/status/containerStatuses").iter().find_map(|status| {
            let reason = text(status, "/state/waiting/reason")?;
            let message = text(status, "/state/waiting/message").unwrap_or_default();
            FATAL.contains(&reason).then(|| format!("{reason}: {message}"))
        })
    }

    /// `podIP:port`, with the port named `http` or the first declared one.
    pub fn address(&self) -> Option<SocketAddr> {
        let ip: IpAddr = text(&self.0, "/status/podIP")?.parse().ok()?;
        let ports: Vec<&Value> =
            array(&self.0, "/spec/containers").iter().flat_map(|container| array(container, "/ports")).collect();
        let port = ports.iter().find(|port| text(port, "/name") == Some("http")).or(ports.first())?;
        let port = u16::try_from(port.get("containerPort")?.as_u64()?).ok()?;
        Some(SocketAddr::new(ip, port))
    }
}

/// A ready pod address found in an EndpointSlice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadyAddress {
    pub pod: String,
    pub address: SocketAddr,
}

/// Ready (and not terminating) pod endpoints of a `discovery.k8s.io/v1` EndpointSlice.
pub fn ready_addresses(slice: &Value) -> Vec<ReadyAddress> {
    let ports = array(slice, "/ports");
    let Some(port) = ports
        .iter()
        .find(|port| text(port, "/name") == Some("http"))
        .or(ports.first())
        .and_then(|port| port.get("port")?.as_u64())
        .and_then(|port| u16::try_from(port).ok())
    else {
        return Vec::new();
    };
    array(slice, "/endpoints")
        .iter()
        .filter(|endpoint| {
            let condition = |key: &str| endpoint.pointer(&format!("/conditions/{key}")).and_then(Value::as_bool);
            condition("ready") != Some(false) && condition("terminating") != Some(true)
        })
        .filter_map(|endpoint| {
            if text(endpoint, "/targetRef/kind") != Some("Pod") {
                return None;
            }
            let pod = text(endpoint, "/targetRef/name")?.to_owned();
            let ip: IpAddr = array(endpoint, "/addresses").first()?.as_str()?.parse().ok()?;
            Some(ReadyAddress { pod, address: SocketAddr::new(ip, port) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ready_addresses_skip_unready_terminating_and_foreign_targets() {
        let slice = json!({
            "metadata": {"name": "svc-abc"},
            "ports": [{"name": "metrics", "port": 9090}, {"name": "http", "port": 8080}],
            "endpoints": [
                {"addresses": ["10.0.0.1"], "conditions": {"ready": true}, "targetRef": {"kind": "Pod", "name": "p1"}},
                {"addresses": ["10.0.0.2"], "conditions": {"ready": false}, "targetRef": {"kind": "Pod", "name": "p2"}},
                {"addresses": ["10.0.0.3"], "conditions": {"ready": true, "terminating": true}, "targetRef": {"kind": "Pod", "name": "p3"}},
                {"addresses": ["10.0.0.4"], "conditions": {}, "targetRef": {"kind": "Node", "name": "n"}},
                {"addresses": ["fd00::5"], "conditions": {}, "targetRef": {"kind": "Pod", "name": "p5"}}
            ]
        });
        assert_eq!(
            ready_addresses(&slice),
            vec![
                ReadyAddress { pod: "p1".into(), address: "10.0.0.1:8080".parse().unwrap() },
                ReadyAddress { pod: "p5".into(), address: "[fd00::5]:8080".parse().unwrap() },
            ]
        );
    }

    #[test]
    fn null_lists_read_as_empty() {
        let slice = json!({"metadata": {"name": "s", "labels": null}, "ports": null, "endpoints": null});
        assert!(ready_addresses(&slice).is_empty());
        assert!(items(&json!({"metadata": {}, "items": null})).is_empty());
        assert_eq!(spec_replicas(&json!({"spec": {}})), 0);
    }

    #[test]
    fn pod_status_helpers() {
        let pod = Pod(json!({
            "metadata": {"name": "p", "annotations": {POD_DELETION_COST: "-5"}, "deletionTimestamp": null},
            "spec": {"containers": [{"ports": [{"containerPort": 8080, "name": "http"}]}]},
            "status": {
                "phase": "Pending",
                "podIP": "10.1.2.3",
                "conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/3 nodes are available"}],
                "containerStatuses": [{"state": {"waiting": {"reason": "ImagePullBackOff", "message": "not found"}}}]
            }
        }));
        assert_eq!(pod.name(), "p");
        assert_eq!(pod.annotation(POD_DELETION_COST), Some("-5"));
        assert!(!pod.is_ready() && !pod.is_terminating());
        assert_eq!(pod.unschedulable_message().as_deref(), Some("0/3 nodes are available"));
        assert_eq!(pod.fatal_waiting_reason().as_deref(), Some("ImagePullBackOff: not found"));
        assert_eq!(pod.address(), Some("10.1.2.3:8080".parse().unwrap()));
    }

    #[test]
    fn deployment_replica_failure() {
        let deployment = json!({"status": {"conditions": [
            {"type": "Available", "status": "True"},
            {"type": "ReplicaFailure", "status": "True", "message": "exceeded quota: compute"}
        ]}});
        assert_eq!(replica_failure(&deployment).as_deref(), Some("exceeded quota: compute"));
        assert_eq!(replica_failure(&json!({"status": {"conditions": null}})), None);
    }
}
