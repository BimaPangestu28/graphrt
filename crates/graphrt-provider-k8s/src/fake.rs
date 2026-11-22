//! In-process fake Kubernetes API server for the conformance suite
//! (feature `fake-api-server`, test-only).
//!
//! It models exactly what the provider relies on:
//! - Deployments with a `scale` subresource and `resourceVersion` CAS (409),
//! - a ReplicaSet-like controller: pods are created up to `replicas`, become
//!   ready after `ready_delay`, and scale-in deletes not-ready pods first, then
//!   the lowest `controller.kubernetes.io/pod-deletion-cost`, then the newest,
//! - one EndpointSlice per Deployment (named after it, as the Service would be)
//!   with LIST + WATCH from a `resourceVersion`, bookmarks, and `410 Gone` after
//!   [`FakeApiServer::compact`],
//! - server-side apply of Deployments, Services and Secrets,
//! - bearer-token checks, fault injection (429 / 5xx with `Retry-After`) and a
//!   capacity limit that leaves extra pods `Unschedulable`.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use graphrt_provider_api::conformance::{FakeHandler, FakeResponse, RecordedRequest as ScriptedRequest};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::resources::POD_DELETION_COST;

/// Behaviour knobs of the fake cluster.
#[derive(Debug, Clone)]
pub struct FakeOptions {
    /// Time from pod creation to `Ready`.
    pub ready_delay: Duration,
    /// Pods beyond this many (cluster-wide) stay Pending/Unschedulable.
    pub capacity: Option<usize>,
    /// Pods beyond this many (cluster-wide) are never created, as with an
    /// exhausted ResourceQuota: the Deployment gets `ReplicaFailure=True`.
    pub quota: Option<usize>,
    /// Accepted bearer tokens; `None` accepts any request.
    pub accepted_tokens: Option<HashSet<String>>,
}

impl Default for FakeOptions {
    fn default() -> Self {
        Self { ready_delay: Duration::from_millis(20), capacity: None, quota: None, accepted_tokens: None }
    }
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub bearer: Option<String>,
}

#[derive(Debug, Clone)]
struct FakePod {
    namespace: String,
    deployment: String,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    ip: String,
    port: u16,
    ready: bool,
    scheduled: bool,
    resource_version: u64,
    serial: u64,
}

#[derive(Debug, Clone)]
struct FakeDeployment {
    replicas: u32,
    resource_version: u64,
    pod_labels: BTreeMap<String, String>,
    port: u16,
    manifest: Value,
    replica_failure: Option<String>,
}

#[derive(Debug, Clone)]
struct SliceEvent {
    resource_version: u64,
    namespace: String,
    service: String,
    kind: &'static str,
    object: Value,
}

#[derive(Debug, Default)]
struct Cluster {
    resource_version: u64,
    pod_serial: u64,
    deployments: BTreeMap<(String, String), FakeDeployment>,
    pods: BTreeMap<(String, String), FakePod>,
    applied: BTreeMap<(String, String, String), Value>,
    slices: BTreeMap<(String, String), Value>,
    history: Vec<SliceEvent>,
    compacted_below: u64,
    faults: VecDeque<(u16, Option<u64>)>,
    requests: Vec<RecordedRequest>,
    scale_patches: usize,
    scale_conflicts: usize,
    accepted_tokens: Option<HashSet<String>>,
}

#[derive(Debug)]
struct Shared {
    cluster: Mutex<Cluster>,
    events: broadcast::Sender<SliceEvent>,
    options: FakeOptions,
}

/// A running fake API server; dropped = stopped.
#[derive(Debug)]
pub struct FakeApiServer {
    address: SocketAddr,
    shared: Arc<Shared>,
    _shutdown: oneshot::Sender<()>,
}

impl FakeApiServer {
    /// Start on `127.0.0.1:<random port>`.
    ///
    /// @throws std::io::Error - when the listener cannot bind
    pub async fn start(options: FakeOptions) -> std::io::Result<Self> {
        let (events, _) = broadcast::channel(4096);
        let cluster = Cluster { accepted_tokens: options.accepted_tokens.clone(), ..Cluster::default() };
        let shared = Arc::new(Shared { cluster: Mutex::new(cluster), events, options });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (shutdown, stopped) = oneshot::channel::<()>();
        let state = shared.clone();
        let router = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let state = state.clone();
            async move {
                let bearer = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .map(str::to_owned);
                let request = RecordedRequest {
                    method: method.to_string(),
                    path: uri.path().to_owned(),
                    query: uri.query().unwrap_or_default().to_owned(),
                    bearer,
                };
                match handle(&state, request, &body) {
                    Handled::Reply(reply) => into_axum(reply),
                    Handled::Watch { namespace, service, query } => watch_slices(&state, &namespace, &service, &query),
                }
            }
        });
        tokio::spawn(async move {
            let serve = axum::serve(listener, router).with_graceful_shutdown(async {
                let _stopped = stopped.await;
            });
            if let Err(error) = serve.await {
                tracing::warn!(%error, "fake API server stopped");
            }
        });
        Ok(Self { address, shared, _shutdown: shutdown })
    }

    /// The same emulated cluster as a handler for the shared conformance
    /// harness's `FakeServer` (one request per connection: a watch answers
    /// its backlog and closes, and the provider re-watches).
    pub fn scripted_handler(&self) -> FakeHandler {
        let shared = self.shared.clone();
        Arc::new(move |request: &ScriptedRequest| {
            let (path, query) = request.path.split_once('?').unwrap_or((request.path.as_str(), ""));
            let bearer = request.header("authorization").and_then(|value| value.strip_prefix("Bearer ")).map(str::to_owned);
            let recorded =
                RecordedRequest { method: request.method.clone(), path: path.to_owned(), query: query.to_owned(), bearer };
            let reply = match handle(&shared, recorded, &request.body) {
                Handled::Reply(reply) => reply,
                Handled::Watch { namespace, service, query } => {
                    let lines = watch_backlog(&shared, &namespace, &service, watch_since(&query)).unwrap_or_else(|gone| vec![gone]);
                    Reply { status: 200, body: lines.concat(), retry_after: None }
                }
            };
            let mut response = FakeResponse { status: reply.status, headers: Vec::new(), body: reply.body };
            response.headers.push(("content-type".into(), "application/json".into()));
            if let Some(seconds) = reply.retry_after {
                response.headers.push(("retry-after".into(), seconds.to_string()));
            }
            response
        })
    }

    /// Base URL (`http://127.0.0.1:<port>`).
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn cluster(&self) -> MutexGuard<'_, Cluster> {
        lock(&self.shared)
    }

    /// Seed a Deployment directly (as `graphrt deploy` would have created it).
    pub fn add_deployment(&self, namespace: &str, name: &str, deployment: &str, version: &str, port: u16) {
        let pod_labels = BTreeMap::from([
            ("graphrt.dev/deployment".to_owned(), deployment.to_owned()),
            ("graphrt.dev/version".to_owned(), version.to_owned()),
        ]);
        let mut cluster = self.cluster();
        let resource_version = cluster.bump();
        cluster.deployments.insert(
            (namespace.into(), name.into()),
            FakeDeployment { replicas: 0, resource_version, pod_labels, port, manifest: Value::Null, replica_failure: None },
        );
        cluster.publish_slice(&self.shared, namespace, name);
    }

    /// Spec replicas of a Deployment.
    pub fn replicas(&self, namespace: &str, name: &str) -> Option<u32> {
        self.cluster().deployments.get(&(namespace.into(), name.into())).map(|deployment| deployment.replicas)
    }

    /// Names of the live pods of a Deployment.
    pub fn pods(&self, namespace: &str, name: &str) -> Vec<String> {
        self.cluster()
            .pods
            .iter()
            .filter(|((pod_namespace, _), pod)| pod_namespace == namespace && pod.deployment == name)
            .map(|((_, pod), _)| pod.clone())
            .collect()
    }

    /// Kill a pod behind the gateway's back (node failure, `kubectl delete`).
    pub fn kill_pod(&self, namespace: &str, pod: &str) {
        let mut cluster = self.cluster();
        if let Some(removed) = cluster.pods.remove(&(namespace.into(), pod.into())) {
            cluster.reconcile(&self.shared, namespace, &removed.deployment);
        }
    }

    /// Answer the next requests with these statuses (`Retry-After` seconds optional).
    pub fn inject_faults(&self, faults: impl IntoIterator<Item = (u16, Option<u64>)>) {
        self.cluster().faults.extend(faults);
    }

    /// Answer the next `count` scale patches with 409 Conflict (a concurrent writer).
    pub fn inject_scale_conflicts(&self, count: usize) {
        self.cluster().scale_conflicts += count;
    }

    /// Replace the accepted bearer tokens (simulates token rotation / expiry).
    pub fn set_accepted_tokens(&self, tokens: Option<HashSet<String>>) {
        self.cluster().accepted_tokens = tokens;
    }

    /// Forget watch history: watches from an older `resourceVersion` get `410 Gone`.
    pub fn compact(&self) {
        let mut cluster = self.cluster();
        cluster.compacted_below = cluster.resource_version + 1;
        cluster.history.clear();
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.cluster().requests.clone()
    }

    /// Successful `PATCH .../scale` calls so far.
    pub fn scale_patches(&self) -> usize {
        self.cluster().scale_patches
    }

    /// An object written by server-side apply (`kind` = `deployments`, `services`, `secrets`).
    pub fn applied(&self, namespace: &str, kind: &str, name: &str) -> Option<Value> {
        self.cluster().applied.get(&(namespace.into(), kind.into(), name.into())).cloned()
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, Cluster> {
    shared.cluster.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Cluster {
    fn bump(&mut self) -> u64 {
        self.resource_version += 1;
        self.resource_version
    }

    /// ReplicaSet controller: converge the pods of `name` to its replicas.
    fn reconcile(&mut self, shared: &Arc<Shared>, namespace: &str, name: &str) {
        let Some(deployment) = self.deployments.get(&(namespace.into(), name.into())).cloned() else { return };
        let mut owned: Vec<(String, FakePod)> = self
            .pods
            .iter()
            .filter(|((pod_namespace, _), pod)| pod_namespace == namespace && pod.deployment == name)
            .map(|((_, pod_name), pod)| (pod_name.clone(), pod.clone()))
            .collect();
        let wanted = usize::try_from(deployment.replicas).unwrap_or(usize::MAX);
        let mut failure = None;
        while owned.len() < wanted {
            if shared.options.quota.is_some_and(|quota| self.pods.len() >= quota) {
                failure = Some(format!(
                    "pods \"{name}-x\" is forbidden: exceeded quota: compute, requested: pods=1, used: pods={}, limited: pods={}",
                    self.pods.len(),
                    self.pods.len()
                ));
                break;
            }
            let pod = self.create_pod(shared, namespace, name, &deployment);
            owned.push(pod);
        }
        if let Some(entry) = self.deployments.get_mut(&(namespace.into(), name.into())) {
            entry.replica_failure = failure;
        }
        if owned.len() > wanted {
            // Not-ready first, then lowest deletion cost, then newest (ReplicaSet ranking).
            owned.sort_by_key(|(_, pod)| (pod.ready, deletion_cost(pod), std::cmp::Reverse(pod.serial)));
            for (pod_name, _) in owned.iter().take(owned.len() - wanted) {
                self.pods.remove(&(namespace.into(), pod_name.clone()));
            }
        }
        self.publish_slice(shared, namespace, name);
    }

    fn create_pod(
        &mut self,
        shared: &Arc<Shared>,
        namespace: &str,
        name: &str,
        deployment: &FakeDeployment,
    ) -> (String, FakePod) {
        self.pod_serial += 1;
        let serial = self.pod_serial;
        let pod_name = format!("{name}-{serial:05}");
        let scheduled = shared.options.capacity.is_none_or(|capacity| self.pods.values().filter(|pod| pod.scheduled).count() < capacity);
        let resource_version = self.bump();
        let pod = FakePod {
            namespace: namespace.into(),
            deployment: name.into(),
            labels: deployment.pod_labels.clone(),
            annotations: BTreeMap::new(),
            ip: format!("10.{}.{}.{}", (serial >> 16) & 0xff, (serial >> 8) & 0xff, serial & 0xff),
            port: deployment.port,
            ready: false,
            scheduled,
            resource_version,
            serial,
        };
        self.pods.insert((namespace.into(), pod_name.clone()), pod.clone());
        if scheduled {
            let shared = shared.clone();
            let key = (namespace.to_owned(), pod_name.clone());
            let delay = shared.options.ready_delay;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let mut cluster = lock(&shared);
                let resource_version = cluster.bump();
                let Some(pod) = cluster.pods.get_mut(&key) else { return };
                pod.ready = true;
                pod.resource_version = resource_version;
                let deployment = pod.deployment.clone();
                cluster.publish_slice(&shared, &key.0, &deployment);
            });
        }
        (pod_name, pod)
    }

    /// Recompute the EndpointSlice of `service`; record + broadcast a change.
    fn publish_slice(&mut self, shared: &Shared, namespace: &str, service: &str) {
        let Some(deployment) = self.deployments.get(&(namespace.into(), service.into())) else { return };
        let mut endpoints: Vec<(u64, Value)> = self
            .pods
            .iter()
            .filter(|((pod_namespace, _), pod)| pod_namespace == namespace && pod.deployment == service && pod.scheduled)
            .map(|((_, pod_name), pod)| {
                (
                    pod.serial,
                    json!({
                        "addresses": [pod.ip],
                        "conditions": { "ready": pod.ready, "serving": pod.ready, "terminating": false },
                        "targetRef": { "kind": "Pod", "name": pod_name, "namespace": namespace },
                    }),
                )
            })
            .collect();
        endpoints.sort_by_key(|(serial, _)| *serial);
        let endpoints: Vec<Value> = endpoints.into_iter().map(|(_, endpoint)| endpoint).collect();
        let port = deployment.port;
        let key = (namespace.to_owned(), service.to_owned());
        let previous = self.slices.get(&key).cloned();
        if previous.as_ref().is_some_and(|slice| slice.get("endpoints") == Some(&if endpoints.is_empty() { Value::Null } else { json!(endpoints) })) {
            return;
        }
        let resource_version = self.bump();
        let slice = json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": format!("{service}-fake"),
                "namespace": namespace,
                "resourceVersion": resource_version.to_string(),
                "labels": { "kubernetes.io/service-name": service },
            },
            "addressType": "IPv4",
            "ports": [{ "name": "http", "port": port, "protocol": "TCP" }],
            // A real API server sends `null`, not `[]`, for a slice without endpoints.
            "endpoints": if endpoints.is_empty() { Value::Null } else { json!(endpoints) },
        });
        self.slices.insert(key, slice.clone());
        let event = SliceEvent {
            resource_version,
            namespace: namespace.into(),
            service: service.into(),
            kind: if previous.is_some() { "MODIFIED" } else { "ADDED" },
            object: slice,
        };
        self.history.push(event.clone());
        let _no_watchers = shared.events.send(event);
    }
}

fn deletion_cost(pod: &FakePod) -> i64 {
    pod.annotations.get(POD_DELETION_COST).and_then(|cost| cost.parse().ok()).unwrap_or(0)
}

fn pod_json(name: &str, pod: &FakePod) -> Value {
    let mut conditions = vec![json!({ "type": "Ready", "status": if pod.ready { "True" } else { "False" } })];
    if !pod.scheduled {
        conditions.push(json!({
            "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
            "message": "0/1 nodes are available: insufficient cpu",
        }));
    }
    json!({
        "metadata": {
            "name": name, "namespace": pod.namespace, "resourceVersion": pod.resource_version.to_string(),
            "labels": pod.labels, "annotations": pod.annotations,
        },
        "spec": { "containers": [{ "name": "agent", "ports": [{ "name": "http", "containerPort": pod.port }] }] },
        "status": {
            "phase": if pod.scheduled { "Running" } else { "Pending" },
            "podIP": if pod.scheduled { Value::from(pod.ip.clone()) } else { Value::Null },
            "conditions": conditions,
        },
    })
}

/// A non-streaming answer of the emulated API.
#[derive(Debug)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<u64>,
}

/// What a request resolves to: an answer, or a watch to serve.
#[derive(Debug)]
enum Handled {
    Reply(Reply),
    Watch { namespace: String, service: String, query: String },
}

fn respond(status: StatusCode, body: &Value) -> Reply {
    Reply { status: status.as_u16(), body: body.to_string().into_bytes(), retry_after: None }
}

fn status_response(code: u16, reason: &str, message: &str) -> Reply {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    respond(status, &json!({ "kind": "Status", "status": "Failure", "code": code, "reason": reason, "message": message }))
}

fn into_axum(reply: Reply) -> Response {
    let mut response = Response::new(Body::from(reply.body));
    *response.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert("content-type", axum::http::HeaderValue::from_static("application/json"));
    if let Some(seconds) = reply.retry_after
        && let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(byte) = bytes.get(index) {
        let hex = bytes.get(index + 1..index + 3).and_then(|hex| std::str::from_utf8(hex).ok());
        match (byte, hex.and_then(|hex| u8::from_str_radix(hex, 16).ok())) {
            (b'%', Some(value)) => {
                decoded.push(value);
                index += 3;
            }
            _ => {
                decoded.push(*byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn selector_matches(selector: &str, labels: &BTreeMap<String, String>) -> bool {
    selector.split(',').filter(|term| !term.is_empty()).all(|term| match term.split_once('=') {
        Some((key, value)) => labels.get(key).map(String::as_str) == Some(value),
        None => labels.contains_key(term),
    })
}

fn handle(shared: &Arc<Shared>, request: RecordedRequest, body: &[u8]) -> Handled {
    let RecordedRequest { method, path, query, bearer } = request.clone();
    let mut cluster = lock(shared);
    cluster.requests.push(request);
    if let Some(accepted) = &cluster.accepted_tokens
        && !bearer.as_ref().is_some_and(|token| accepted.contains(token))
    {
        return Handled::Reply(status_response(401, "Unauthorized", "Unauthorized"));
    }
    if let Some((code, retry_after)) = cluster.faults.pop_front() {
        return Handled::Reply(Reply { retry_after, ..status_response(code, "Injected", "injected fault") });
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let reply = match (method.as_str(), segments.as_slice()) {
        ("GET", ["apis", "apps", "v1", "namespaces", namespace, "deployments", name, "scale"]) => {
            get_scale(&cluster, namespace, name)
        }
        ("PATCH", ["apis", "apps", "v1", "namespaces", namespace, "deployments", name, "scale"]) => {
            patch_scale(&mut cluster, shared, namespace, name, body)
        }
        ("GET", ["apis", "apps", "v1", "namespaces", namespace, "deployments", name]) => {
            match cluster.deployments.get(&((*namespace).into(), (*name).into())) {
                Some(deployment) => {
                    let conditions: Vec<Value> = deployment
                        .replica_failure
                        .iter()
                        .map(|message| json!({ "type": "ReplicaFailure", "status": "True", "reason": "FailedCreate", "message": message }))
                        .collect();
                    respond(StatusCode::OK, &json!({
                        "metadata": { "name": name, "namespace": namespace },
                        "spec": { "replicas": deployment.replicas },
                        "status": { "conditions": conditions },
                    }))
                }
                None => status_response(404, "NotFound", &format!("deployments.apps \"{name}\" not found")),
            }
        }
        ("PATCH", ["apis", "apps", "v1", "namespaces", namespace, "deployments", name]) => {
            apply_deployment(&mut cluster, shared, namespace, name, body)
        }
        ("PATCH", ["api", "v1", "namespaces", namespace, kind @ ("services" | "secrets"), name]) => {
            apply_object(&mut cluster, namespace, kind, name, body)
        }
        ("GET", ["api", "v1", "namespaces", namespace, "pods"]) => {
            let selector = query_param(&query, "labelSelector").unwrap_or_default();
            let items: Vec<Value> = cluster
                .pods
                .iter()
                .filter(|((pod_namespace, _), pod)| pod_namespace == namespace && selector_matches(&selector, &pod.labels))
                .map(|((_, name), pod)| pod_json(name, pod))
                .collect();
            let resource_version = cluster.resource_version.to_string();
            respond(StatusCode::OK, &json!({ "kind": "PodList", "metadata": { "resourceVersion": resource_version }, "items": items }))
        }
        ("GET", ["api", "v1", "namespaces", namespace, "pods", name]) => {
            match cluster.pods.get(&((*namespace).into(), (*name).into())) {
                Some(pod) => respond(StatusCode::OK, &pod_json(name, pod)),
                None => status_response(404, "NotFound", &format!("pods \"{name}\" not found")),
            }
        }
        ("PATCH", ["api", "v1", "namespaces", namespace, "pods", name]) => patch_pod(&mut cluster, namespace, name, body),
        ("GET", ["apis", "discovery.k8s.io", "v1", "namespaces", namespace, "endpointslices"]) => {
            let selector = query_param(&query, "labelSelector").unwrap_or_default();
            let service = selector.strip_prefix("kubernetes.io/service-name=").unwrap_or_default().to_owned();
            if query_param(&query, "watch").as_deref() == Some("1") {
                return Handled::Watch { namespace: (*namespace).to_owned(), service, query };
            }
            let items: Vec<Value> = cluster.slices.get(&((*namespace).into(), service)).cloned().into_iter().collect();
            let resource_version = cluster.resource_version.to_string();
            respond(StatusCode::OK, &json!({ "kind": "EndpointSliceList", "metadata": { "resourceVersion": resource_version }, "items": items }))
        }
        _ => status_response(404, "NotFound", &format!("fake API server: no route for {method} {path}")),
    };
    Handled::Reply(reply)
}

fn get_scale(cluster: &Cluster, namespace: &str, name: &str) -> Reply {
    match cluster.deployments.get(&(namespace.into(), name.into())) {
        Some(deployment) => respond(
            StatusCode::OK,
            &json!({
                "kind": "Scale", "apiVersion": "autoscaling/v1",
                "metadata": { "name": name, "namespace": namespace, "resourceVersion": deployment.resource_version.to_string() },
                "spec": { "replicas": deployment.replicas },
            }),
        ),
        None => status_response(404, "NotFound", &format!("deployments.apps \"{name}\" not found")),
    }
}

fn patch_scale(cluster: &mut Cluster, shared: &Arc<Shared>, namespace: &str, name: &str, body: &[u8]) -> Reply {
    let Ok(patch) = serde_json::from_slice::<Value>(body) else {
        return status_response(400, "BadRequest", "invalid merge patch");
    };
    if cluster.scale_conflicts > 0 {
        cluster.scale_conflicts -= 1;
        return status_response(409, "Conflict", "the object has been modified (injected)");
    }
    let resource_version = cluster.resource_version + 1;
    let Some(deployment) = cluster.deployments.get_mut(&(namespace.into(), name.into())) else {
        return status_response(404, "NotFound", &format!("deployments.apps \"{name}\" not found"));
    };
    if let Some(expected) = patch.pointer("/metadata/resourceVersion").and_then(Value::as_str)
        && expected != deployment.resource_version.to_string()
    {
        return status_response(409, "Conflict", "the object has been modified; please apply your changes to the latest version");
    }
    let Some(replicas) = patch.pointer("/spec/replicas").and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok())
    else {
        return status_response(422, "Invalid", "spec.replicas missing");
    };
    deployment.replicas = replicas;
    deployment.resource_version = resource_version;
    cluster.resource_version = resource_version;
    cluster.scale_patches += 1;
    cluster.reconcile(shared, namespace, name);
    get_scale(cluster, namespace, name)
}

fn apply_deployment(cluster: &mut Cluster, shared: &Arc<Shared>, namespace: &str, name: &str, body: &[u8]) -> Reply {
    let Ok(manifest) = serde_json::from_slice::<Value>(body) else {
        return status_response(400, "BadRequest", "invalid apply patch");
    };
    let pod_labels: BTreeMap<String, String> = manifest
        .pointer("/spec/template/metadata/labels")
        .cloned()
        .and_then(|labels| serde_json::from_value(labels).ok())
        .unwrap_or_default();
    let port = manifest
        .pointer("/spec/template/spec/containers/0/ports/0/containerPort")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(8080);
    let replicas = manifest.pointer("/spec/replicas").and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok());
    let resource_version = cluster.bump();
    let key = (namespace.to_owned(), name.to_owned());
    let entry = cluster.deployments.entry(key).or_insert_with(|| FakeDeployment {
        replicas: 0,
        resource_version,
        pod_labels: pod_labels.clone(),
        port,
        manifest: Value::Null,
        replica_failure: None,
    });
    entry.replicas = replicas.unwrap_or(entry.replicas);
    entry.resource_version = resource_version;
    entry.pod_labels = pod_labels;
    entry.port = port;
    entry.manifest = manifest.clone();
    cluster.applied.insert((namespace.into(), "deployments".into(), name.into()), manifest.clone());
    cluster.reconcile(shared, namespace, name);
    respond(StatusCode::OK, &manifest)
}

fn apply_object(cluster: &mut Cluster, namespace: &str, kind: &str, name: &str, body: &[u8]) -> Reply {
    let Ok(manifest) = serde_json::from_slice::<Value>(body) else {
        return status_response(400, "BadRequest", "invalid apply patch");
    };
    cluster.bump();
    cluster.applied.insert((namespace.into(), kind.into(), name.into()), manifest.clone());
    respond(StatusCode::OK, &manifest)
}

fn patch_pod(cluster: &mut Cluster, namespace: &str, name: &str, body: &[u8]) -> Reply {
    let Ok(patch) = serde_json::from_slice::<Value>(body) else {
        return status_response(400, "BadRequest", "invalid merge patch");
    };
    let resource_version = cluster.bump();
    let Some(pod) = cluster.pods.get_mut(&(namespace.into(), name.into())) else {
        return status_response(404, "NotFound", &format!("pods \"{name}\" not found"));
    };
    if let Some(annotations) = patch.pointer("/metadata/annotations").and_then(Value::as_object) {
        for (key, value) in annotations {
            match value.as_str() {
                Some(value) => pod.annotations.insert(key.clone(), value.to_owned()),
                None => pod.annotations.remove(key),
            };
        }
    }
    pod.resource_version = resource_version;
    respond(StatusCode::OK, &pod_json(name, pod))
}

/// Watch history after `resourceVersion` (or a `410 Gone`), then a bookmark.
/// `Err` carries the 410 line.
fn watch_backlog(shared: &Shared, namespace: &str, service: &str, since: u64) -> Result<Vec<Bytes>, Bytes> {
    let cluster = lock(shared);
    if since != 0 && since < cluster.compacted_below {
        let gone = json!({ "type": "ERROR", "object": {
            "kind": "Status", "status": "Failure", "code": 410, "reason": "Expired",
            "message": format!("too old resource version: {since}"),
        }});
        return Err(Bytes::from(format!("{gone}\n")));
    }
    let mut lines: Vec<Bytes> = cluster
        .history
        .iter()
        .filter(|event| event.resource_version > since && event.namespace == namespace && event.service == service)
        .map(event_line)
        .collect();
    let bookmark = json!({ "type": "BOOKMARK", "object": {
        "kind": "EndpointSlice", "metadata": { "resourceVersion": cluster.resource_version.to_string() },
    }});
    lines.push(Bytes::from(format!("{bookmark}\n")));
    Ok(lines)
}

fn watch_since(query: &str) -> u64 {
    query_param(query, "resourceVersion").and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// A streaming watch: backlog, then live events until `timeoutSeconds`.
fn watch_slices(shared: &Arc<Shared>, namespace: &str, service: &str, query: &str) -> Response {
    let since = watch_since(query);
    let timeout = Duration::from_secs(query_param(query, "timeoutSeconds").and_then(|value| value.parse().ok()).unwrap_or(30));
    let (sender, receiver) = mpsc::unbounded_channel::<Bytes>();
    // Subscribe before reading the backlog so no event falls between the two.
    let mut live = shared.events.subscribe();
    match watch_backlog(shared, namespace, service, since) {
        Ok(lines) => {
            for line in lines {
                let _closed = sender.send(line);
            }
        }
        Err(gone) => {
            let _closed = sender.send(gone);
            return stream_response(receiver);
        }
    }
    let namespace = namespace.to_owned();
    let service = service.to_owned();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, live.recv()).await {
                Ok(Ok(event)) if event.namespace == namespace && event.service == service && event.resource_version > since => {
                    if sender.send(event_line(&event)).is_err() {
                        return;
                    }
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return,
            }
        }
    });
    stream_response(receiver)
}

fn event_line(event: &SliceEvent) -> Bytes {
    Bytes::from(format!("{}\n", json!({ "type": event.kind, "object": event.object })))
}

fn stream_response(receiver: mpsc::UnboundedReceiver<Bytes>) -> Response {
    let stream = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|line| (Ok::<Bytes, Infallible>(line), receiver))
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert("content-type", axum::http::HeaderValue::from_static("application/json"));
    response
}
