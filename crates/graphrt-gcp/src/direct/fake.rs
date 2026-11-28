//! A stateful fake of the Google APIs a direct Cloud Run deploy calls, for
//! tests of this crate and of the CLI (feature `fake`, never in a binary).
//! Every API is served at one base URL ([`super::Endpoints::all_at`]); the
//! services' public URL is `<base>/public/<service>`, whose `/readyz` answers
//! 200 while the service serves and is not paused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use graphrt_provider_api::conformance::{FakeHandler, FakeResponse, FakeServer, RecordedRequest};
use serde_json::{Value, json};

use super::resources::REQUIRED_APIS;

/// The bearer token the fake accepts.
pub const TOKEN: &str = "ya29.fake-direct-token";
/// The project the fake serves.
pub const PROJECT: &str = "fake-project";
/// Its number (in service URLs).
pub const PROJECT_NUMBER: &str = "123456789";

/// What the fake Google holds.
#[derive(Debug, Default)]
pub struct FakeGoogleState {
    pub enabled_apis: BTreeSet<String>,
    pub service_accounts: BTreeSet<String>,
    pub buckets: BTreeMap<String, Value>,
    pub bucket_policies: BTreeMap<String, Value>,
    pub objects: BTreeMap<(String, String), Vec<u8>>,
    pub secrets: BTreeMap<String, Value>,
    pub secret_versions: BTreeMap<String, Vec<Vec<u8>>>,
    pub secret_policies: BTreeMap<String, Value>,
    pub repositories: BTreeSet<String>,
    pub services: BTreeMap<String, Value>,
    pub revisions: BTreeMap<String, Value>,
    pub revision_counter: u32,
    /// The next revision fails its startup probe.
    pub fail_next_revision: bool,
    /// Calls to this API answer 403 SERVICE_DISABLED and cannot be turned on.
    pub stuck_disabled_api: Option<String>,
    /// Every call answers 403 BILLING_DISABLED.
    pub billing_disabled: bool,
}

/// The fake and its server.
#[derive(Debug)]
pub struct FakeGoogle {
    pub server: FakeServer,
    pub state: Arc<Mutex<FakeGoogleState>>,
}

impl FakeGoogle {
    /// Starts the fake with every API on.
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(FakeGoogleState {
            enabled_apis: REQUIRED_APIS.iter().map(|api| (*api).to_owned()).collect(),
            ..FakeGoogleState::default()
        }));
        let base = Arc::new(Mutex::new(String::new()));
        let handler_state = state.clone();
        let handler_base = base.clone();
        let handler: FakeHandler = Arc::new(move |request: &RecordedRequest| {
            let base = handler_base.lock().unwrap().clone();
            handle(&mut handler_state.lock().unwrap(), &base, request)
        });
        let server = FakeServer::start(handler).await.expect("fake Google starts");
        *base.lock().unwrap() = server.base_url();
        Self { server, state }
    }

    /// The base URL of every API.
    pub fn url(&self) -> String {
        self.server.base_url()
    }

    /// The state, locked.
    pub fn state(&self) -> MutexGuard<'_, FakeGoogleState> {
        self.state.lock().unwrap()
    }

    /// Requests whose path starts with `prefix`.
    pub fn requests_to(&self, method: &str, prefix: &str) -> Vec<RecordedRequest> {
        self.server.requests().into_iter().filter(|r| r.method == method && r.path.starts_with(prefix)).collect()
    }
}

fn google_error(status: u16, grpc: &str, message: &str, reason: Option<(&str, Value)>) -> FakeResponse {
    let mut error = json!({ "code": status, "status": grpc, "message": message });
    if let Some((reason, metadata)) = reason {
        error["details"] =
            json!([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": reason, "metadata": metadata }]);
    }
    FakeResponse::json(status, &json!({ "error": error }))
}

fn not_found() -> FakeResponse {
    google_error(404, "NOT_FOUND", "not found", None)
}

fn done(response: Value) -> FakeResponse {
    FakeResponse::json(200, &json!({ "name": "operations/fake", "done": true, "response": response }))
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| decode(value))
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = value.get(index + 1..index + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap()
}

fn handle(state: &mut FakeGoogleState, base: &str, request: &RecordedRequest) -> FakeResponse {
    let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
    let method = request.method.as_str();
    if let Some(rest) = path.strip_prefix("/public/") {
        let service = rest.trim_end_matches("/readyz");
        let serving = state.services.get(service).is_some_and(|svc| {
            svc["ingress"] == "INGRESS_TRAFFIC_ALL" && svc.get("latestReadyRevision").is_some_and(Value::is_string)
        });
        return FakeResponse::status(if serving { 200 } else { 404 });
    }
    if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return google_error(401, "UNAUTHENTICATED", "Request had invalid authentication credentials.", None);
    }
    if state.billing_disabled {
        return google_error(
            403,
            "PERMISSION_DENIED",
            "This API method requires billing to be enabled.",
            Some(("BILLING_DISABLED", json!({}))),
        );
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let api = if path.starts_with("/v2/") {
        "run.googleapis.com"
    } else if path.starts_with("/storage/") || path.starts_with("/upload/") {
        "storage.googleapis.com"
    } else if path.contains("/secrets") {
        "secretmanager.googleapis.com"
    } else if path.contains("/serviceAccounts") {
        "iam.googleapis.com"
    } else if path.contains("/repositories") {
        "artifactregistry.googleapis.com"
    } else {
        "serviceusage.googleapis.com"
    };
    let disabled = api != "serviceusage.googleapis.com" && !state.enabled_apis.contains(api);
    if disabled || state.stuck_disabled_api.as_deref() == Some(api) {
        return google_error(
            403,
            "PERMISSION_DENIED",
            &format!("{api} has not been used in project {PROJECT_NUMBER} before or it is disabled."),
            Some((
                "SERVICE_DISABLED",
                json!({ "service": api, "activationUrl": format!("https://console.developers.google.com/apis/api/{api}/overview?project={PROJECT_NUMBER}") }),
            )),
        );
    }
    let project_prefix = format!("/v1/projects/{PROJECT}");
    if let Some(rest) = path.strip_prefix("/v2/") {
        return run(state, base, method, rest, query, &body);
    }
    if let Some(rest) = path.strip_prefix("/storage/v1/b") {
        return storage(state, method, rest, query, &body);
    }
    if path.starts_with("/upload/storage/v1/b/") {
        let bucket = path.trim_start_matches("/upload/storage/v1/b/").trim_end_matches("/o").to_owned();
        let name = query_param(query, "name").unwrap_or_default();
        state.objects.insert((bucket, name.clone()), request.body.clone());
        return FakeResponse::json(200, &json!({ "name": name }));
    }
    let Some(rest) = path.strip_prefix(&project_prefix) else { return not_found() };
    if rest == "/services" && method == "GET" {
        let services: Vec<Value> = state
            .enabled_apis
            .iter()
            .map(|api| json!({ "name": format!("projects/{PROJECT_NUMBER}/services/{api}"), "config": { "name": api }, "parent": format!("projects/{PROJECT_NUMBER}") }))
            .collect();
        return FakeResponse::json(200, &json!({ "services": services }));
    }
    if rest == "/services:batchEnable" && method == "POST" {
        for api in body["serviceIds"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if state.stuck_disabled_api.as_deref() != Some(api) {
                state.enabled_apis.insert(api.to_owned());
            }
        }
        return done(json!({}));
    }
    if let Some(account) = rest.strip_prefix("/serviceAccounts") {
        return accounts(state, method, account, &body);
    }
    if let Some(secret) = rest.strip_prefix("/secrets") {
        return secrets(state, method, secret, query, &body);
    }
    if let Some(repository) = rest.strip_prefix("/locations/") {
        let id = query_param(query, "repositoryId")
            .or_else(|| repository.rsplit_once("/repositories/").map(|(_, id)| id.to_owned()))
            .unwrap_or_default();
        return match method {
            "GET" if state.repositories.contains(&id) => FakeResponse::json(200, &json!({ "name": id })),
            "GET" => not_found(),
            _ => {
                state.repositories.insert(id);
                done(json!({}))
            }
        };
    }
    not_found()
}

fn accounts(state: &mut FakeGoogleState, method: &str, rest: &str, body: &Value) -> FakeResponse {
    let email = rest.trim_start_matches('/').to_owned();
    match method {
        "GET" if state.service_accounts.contains(&email) => FakeResponse::json(200, &json!({ "email": email })),
        "GET" => not_found(),
        "POST" => {
            let id = body["accountId"].as_str().unwrap_or_default();
            state.service_accounts.insert(format!("{id}@{PROJECT}.iam.gserviceaccount.com"));
            FakeResponse::json(200, &json!({}))
        }
        "DELETE" if state.service_accounts.remove(&email) => FakeResponse::json(200, &json!({})),
        _ => not_found(),
    }
}

fn storage(state: &mut FakeGoogleState, method: &str, rest: &str, query: &str, body: &Value) -> FakeResponse {
    if rest.is_empty() && method == "POST" {
        let name = body["name"].as_str().unwrap_or_default().to_owned();
        if state.buckets.contains_key(&name) {
            return google_error(409, "", "You already own this bucket.", None);
        }
        state.buckets.insert(name.clone(), body.clone());
        return FakeResponse::json(200, body);
    }
    let rest = rest.trim_start_matches('/');
    let (bucket, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let bucket = bucket.to_owned();
    if !state.buckets.contains_key(&bucket) {
        return not_found();
    }
    match (method, tail) {
        ("GET", "") => FakeResponse::json(200, &state.buckets[&bucket]),
        ("DELETE", "") => {
            if state.objects.keys().any(|(owner, _)| owner == &bucket) {
                return google_error(409, "", "The bucket you tried to delete is not empty.", None);
            }
            state.buckets.remove(&bucket);
            FakeResponse::status(204)
        }
        ("GET", "iam") => {
            FakeResponse::json(200, state.bucket_policies.get(&bucket).unwrap_or(&json!({ "bindings": [] })))
        }
        ("PUT", "iam") => {
            state.bucket_policies.insert(bucket, body.clone());
            FakeResponse::json(200, body)
        }
        ("GET", "o") => {
            let prefix = query_param(query, "prefix").unwrap_or_default();
            let items: Vec<Value> = state
                .objects
                .keys()
                .filter(|(owner, name)| owner == &bucket && name.starts_with(&prefix))
                .map(|(_, name)| json!({ "name": name }))
                .collect();
            FakeResponse::json(200, &json!({ "items": items }))
        }
        (method, object) => {
            let name = decode(object.trim_start_matches("o/"));
            let key = (bucket, name.clone());
            match method {
                "GET" if state.objects.contains_key(&key) => FakeResponse::json(200, &json!({ "name": name })),
                "DELETE" if state.objects.remove(&key).is_some() => FakeResponse::status(204),
                _ => not_found(),
            }
        }
    }
}

fn secrets(state: &mut FakeGoogleState, method: &str, rest: &str, query: &str, body: &Value) -> FakeResponse {
    if rest.is_empty() {
        if method == "POST" {
            let id = query_param(query, "secretId").unwrap_or_default();
            if state.secrets.contains_key(&id) {
                return google_error(409, "ALREADY_EXISTS", "Secret already exists", None);
            }
            let mut secret = body.clone();
            secret["name"] = Value::from(format!("projects/{PROJECT}/secrets/{id}"));
            state.secrets.insert(id, secret.clone());
            return FakeResponse::json(200, &secret);
        }
        let filter = query_param(query, "filter").unwrap_or_default();
        let wanted = filter.strip_prefix("labels.graphrt-service=").unwrap_or_default();
        let listed: Vec<Value> =
            state.secrets.values().filter(|secret| secret["labels"]["graphrt-service"] == wanted).cloned().collect();
        return FakeResponse::json(200, &json!({ "secrets": listed }));
    }
    let rest = rest.trim_start_matches('/');
    let (id, action) = match rest.split_once(':') {
        Some((id, action)) => (id.to_owned(), action),
        None => (rest.split('/').next().unwrap_or_default().to_owned(), ""),
    };
    if !state.secrets.contains_key(&id) {
        return not_found();
    }
    match (method, action) {
        ("POST", "addVersion") => {
            let versions = state.secret_versions.entry(id.clone()).or_default();
            versions.push(body["payload"]["data"].as_str().unwrap_or_default().as_bytes().to_vec());
            let number = versions.len();
            FakeResponse::json(
                200,
                &json!({ "name": format!("projects/{PROJECT_NUMBER}/secrets/{id}/versions/{number}") }),
            )
        }
        ("GET", "getIamPolicy") => FakeResponse::json(200, state.secret_policies.get(&id).unwrap_or(&json!({}))),
        ("POST", "setIamPolicy") => {
            state.secret_policies.insert(id, body["policy"].clone());
            FakeResponse::json(200, &body["policy"])
        }
        ("GET", _) if rest.contains("/versions") => {
            let versions: Vec<Value> = state
                .secret_versions
                .get(&id)
                .map(|versions| {
                    (1..=versions.len()).map(|n| json!({ "name": n.to_string(), "state": "ENABLED" })).collect()
                })
                .unwrap_or_default();
            FakeResponse::json(200, &json!({ "versions": versions }))
        }
        ("DELETE", _) => {
            state.secrets.remove(&id);
            state.secret_versions.remove(&id);
            FakeResponse::json(200, &json!({}))
        }
        _ => not_found(),
    }
}

fn run(state: &mut FakeGoogleState, base: &str, method: &str, rest: &str, query: &str, body: &Value) -> FakeResponse {
    let parts: Vec<&str> = rest.split('/').collect();
    // projects/{p}/locations/{r}/services/{s}[/revisions]
    let (Some(region), Some(name)) = (parts.get(3), parts.get(5)) else { return not_found() };
    let name = (*name).to_owned();
    let resource = format!("projects/{PROJECT}/locations/{region}/services/{name}");
    if parts.get(6) == Some(&"revisions") {
        let revisions: Vec<Value> =
            state.revisions.values().filter(|revision| revision["service"] == name.as_str()).cloned().collect();
        return FakeResponse::json(200, &json!({ "revisions": revisions }));
    }
    match method {
        "GET" => state.services.get(&name).map_or_else(not_found, |service| FakeResponse::json(200, service)),
        "DELETE" => match state.services.remove(&name) {
            Some(service) => {
                state.revisions.retain(|_, revision| revision["service"] != name.as_str());
                done(service)
            }
            None => not_found(),
        },
        "PATCH" => {
            let mask = query_param(query, "updateMask");
            let exists = state.services.contains_key(&name);
            if !exists && mask.is_some() {
                return not_found();
            }
            let mut service = state.services.get(&name).cloned().unwrap_or_else(|| {
                json!({
                    "name": resource,
                    "uri": format!("{base}/public/{name}"),
                })
            });
            match mask {
                Some(mask) => {
                    for field in mask.split(',') {
                        service[field] = body[field].clone();
                    }
                }
                None => {
                    let changed = service.get("template") != body.get("template");
                    for (key, value) in body.as_object().into_iter().flatten() {
                        service[key.as_str()] = value.clone();
                    }
                    if changed {
                        state.revision_counter += 1;
                        let revision = format!("{name}-{:05}-abc", state.revision_counter);
                        let template = &body["template"];
                        state.revisions.insert(
                            revision.clone(),
                            json!({
                                "name": format!("{resource}/revisions/{revision}"),
                                "service": name,
                                "labels": template["labels"],
                                "annotations": template["annotations"],
                                "createTime": format!("2026-11-28T10:00:{:02}.000000Z", state.revision_counter),
                            }),
                        );
                        service["latestCreatedRevision"] = Value::from(format!("{resource}/revisions/{revision}"));
                        if std::mem::take(&mut state.fail_next_revision) {
                            service["terminalCondition"] = json!({ "state": "CONDITION_FAILED", "message": "The user-provided container failed the configured startup probe checks." });
                            state.services.insert(name, service);
                            return FakeResponse::json(
                                200,
                                &json!({
                                    "name": "operations/fake", "done": true,
                                    "error": { "code": 9, "message": "The user-provided container failed the configured startup probe checks." },
                                }),
                            );
                        }
                        service["latestReadyRevision"] = Value::from(format!("{resource}/revisions/{revision}"));
                        service["terminalCondition"] = json!({ "state": "CONDITION_SUCCEEDED" });
                    }
                }
            }
            state.services.insert(name, service.clone());
            done(service)
        }
        _ => not_found(),
    }
}
