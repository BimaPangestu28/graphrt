//! In-process fake of the Azure endpoints the crate calls: Entra token
//! endpoint, managed identity (`IDENTITY_ENDPOINT` and IMDS), ARM
//! `Microsoft.App/containerApps` (with long-running operations), Key Vault
//! set-secret, and a Container Apps ingress that scales a revision from zero
//! on the first request. Fault injection: queued statuses, token revocation.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug, Default)]
pub struct Revision {
    pub active: bool,
    pub created: String,
    pub replicas: u64,
}

#[derive(Debug, Default)]
pub struct App {
    pub body: Value,
    pub revisions: Vec<(String, Revision)>,
}

#[derive(Debug, Default)]
pub struct FakeState {
    /// Tokens currently accepted by ARM / Key Vault.
    pub valid_tokens: HashSet<String>,
    pub issued: u64,
    /// `expires_in` of issued Entra / IMDS tokens.
    pub expires_in: u64,
    /// Requests per route key (`token`, `msi`, `imds`, `arm GET app`, `probe`, …).
    pub counts: HashMap<String, u64>,
    /// Statuses returned (with `Retry-After: 0`) before any ARM request is served.
    pub arm_faults: VecDeque<u16>,
    /// IMDS answers 410 this many times first (instance metadata still starting).
    pub imds_booting: u32,
    pub apps: HashMap<String, App>,
    pub operations: HashMap<String, u32>,
    pub secrets: HashMap<String, String>,
    pub cold_start: Duration,
    /// Every request line and body seen, to scan for leaked secrets.
    pub log: Vec<String>,
    pub last_form: HashMap<String, String>,
    /// ARM rejects every token (a principal whose role was removed).
    pub arm_always_401: bool,
}

#[derive(Clone, Debug)]
pub struct FakeAzure {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeAzure {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(FakeState { expires_in: 3600, ..FakeState::default() }));
        let router = Router::new()
            .route("/{tenant}/oauth2/v2.0/token", axum::routing::post(entra_token))
            .route("/msi/token", axum::routing::get(msi_token))
            .route("/metadata/identity/oauth2/token", axum::routing::get(imds_token))
            .route("/operations/{id}", axum::routing::get(operation))
            .route("/secrets/{name}", axum::routing::put(set_secret))
            .fallback(fallback)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { addr, state }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn count(&self, key: &str) -> u64 {
        self.state.lock().unwrap().counts.get(key).copied().unwrap_or(0)
    }

    pub fn reset_counts(&self) {
        self.state.lock().unwrap().counts.clear();
    }

    pub fn with<R>(&self, edit: impl FnOnce(&mut FakeState) -> R) -> R {
        edit(&mut self.state.lock().unwrap())
    }

    /// Insert an existing app with `versions` (all labelled, newest at 100 %).
    pub fn seed_app(&self, app: &str, versions: &[(u64, &str)]) {
        let newest = versions.iter().map(|(version, _)| *version).max().unwrap_or(0);
        let traffic: Vec<Value> = versions
            .iter()
            .map(|(version, _)| {
                json!({"revisionName": format!("{app}--v{version}"), "label": format!("v{version}"),
                       "weight": if *version == newest { 100 } else { 0 }})
            })
            .collect();
        let body = json!({
            "name": app,
            "properties": {
                "provisioningState": "Succeeded",
                "latestRevisionName": format!("{app}--v{newest}"),
                "configuration": {"ingress": {"fqdn": format!("{app}.internal.fake-env.westeurope.azurecontainerapps.io"), "traffic": traffic}},
            }
        });
        let revisions = versions
            .iter()
            .map(|(version, created)| {
                (format!("{app}--v{version}"), Revision { active: true, created: (*created).to_string(), replicas: 0 })
            })
            .collect();
        self.with(|state| state.apps.insert(app.to_string(), App { body, revisions }));
    }
}

fn bump(state: &mut FakeState, key: &str) {
    *state.counts.entry(key.to_string()).or_default() += 1;
}

fn issue(state: &mut FakeState) -> String {
    state.issued += 1;
    let token = format!("fake-token-{}", state.issued);
    state.valid_tokens.insert(token.clone());
    token
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&').filter_map(|pair| pair.split_once('=')).map(|(key, value)| (decode(key), decode(value))).collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            index += 3;
        } else {
            out.push(if bytes[index] == b'+' { b' ' } else { bytes[index] });
            index += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

async fn entra_token(State(state): State<Arc<Mutex<FakeState>>>, Path(tenant): Path<String>, body: String) -> Response {
    let mut state = state.lock().unwrap();
    bump(&mut state, "token");
    state.log.push(format!("POST /{tenant}/oauth2/v2.0/token {body}"));
    let form = parse_form(&body);
    state.last_form = form.clone();
    let has_secret = form.get("client_secret").is_some_and(|secret| secret == "right-secret");
    let has_assertion = form.get("client_assertion_type").map(String::as_str)
        == Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
        && form.get("client_assertion").is_some_and(|assertion| assertion.split('.').count() == 3);
    if form.get("grant_type").map(String::as_str) != Some("client_credentials") || !(has_secret || has_assertion) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided."})),
        )
            .into_response();
    }
    let token = issue(&mut state);
    axum::Json(json!({"token_type": "Bearer", "expires_in": state.expires_in, "access_token": token})).into_response()
}

async fn msi_token(
    State(state): State<Arc<Mutex<FakeState>>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    bump(&mut state, "msi");
    if headers.get("x-identity-header").and_then(|value| value.to_str().ok()) != Some("identity-header-secret")
        || query.get("api-version").map(String::as_str) != Some("2019-08-01")
        || !query.contains_key("resource")
    {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_request"}))).into_response();
    }
    let token = issue(&mut state);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    axum::Json(json!({"access_token": token, "expires_on": (now + state.expires_in).to_string(),
                      "resource": query["resource"], "token_type": "Bearer"}))
    .into_response()
}

async fn imds_token(State(state): State<Arc<Mutex<FakeState>>>, headers: HeaderMap) -> Response {
    let mut state = state.lock().unwrap();
    bump(&mut state, "imds");
    if headers.get("metadata").and_then(|value| value.to_str().ok()) != Some("true") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if state.imds_booting > 0 {
        state.imds_booting -= 1;
        return StatusCode::GONE.into_response();
    }
    let token = issue(&mut state);
    axum::Json(json!({"access_token": token, "expires_in": state.expires_in.to_string(), "token_type": "Bearer"}))
        .into_response()
}

async fn operation(State(state): State<Arc<Mutex<FakeState>>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let mut state = state.lock().unwrap();
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    bump(&mut state, "operation poll");
    let remaining = state.operations.entry(id).or_insert(1);
    if *remaining > 0 {
        *remaining -= 1;
        return axum::Json(json!({"status": "InProgress"})).into_response();
    }
    axum::Json(json!({"status": "Succeeded"})).into_response()
}

async fn set_secret(
    State(state): State<Arc<Mutex<FakeState>>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    bump(&mut state, "keyvault set");
    let value: Value = serde_json::from_slice(&body).unwrap_or_default();
    state.secrets.insert(name.clone(), value["value"].as_str().unwrap_or("").to_string());
    axum::Json(json!({"id": format!("https://kv.vault.azure.net/secrets/{name}/abc")})).into_response()
}

fn authorized(state: &FakeState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| state.valid_tokens.contains(token))
}

/// ARM routes and the fake ingress (by `Host`).
async fn fallback(
    State(shared): State<Arc<Mutex<FakeState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let host = headers.get("host").and_then(|value| value.to_str().ok()).unwrap_or("").to_string();
    if host.contains("---") {
        return ingress(shared, &host, uri.path()).await;
    }
    let mut state = shared.lock().unwrap();
    state.log.push(format!("{method} {uri} {}", String::from_utf8_lossy(&body)));
    if state.arm_always_401 || !authorized(&state, &headers) {
        bump(&mut state, "arm 401");
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"code": "InvalidAuthenticationToken", "message": "expired"}})),
        )
            .into_response();
    }
    if let Some(status) = state.arm_faults.pop_front() {
        bump(&mut state, "arm fault");
        let code = if status == 429 { "TooManyRequests" } else { "InternalServerError" };
        return (
            StatusCode::from_u16(status).unwrap(),
            [("retry-after", "0")],
            axum::Json(json!({"error": {"code": code, "message": "injected"}})),
        )
            .into_response();
    }
    let path = uri.path();
    assert_eq!(
        uri.query().and_then(|query| query.split('&').find(|p| p.starts_with("api-version="))),
        Some("api-version=2025-01-01"),
        "ARM request without the expected api-version: {uri}"
    );
    let Some(rest) = path.split("/providers/Microsoft.App/containerApps/").nth(1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let app = segments[0].to_string();
    let base = format!("http://{host}");
    match (method.as_str(), &segments[1..]) {
        ("GET", []) => {
            bump(&mut state, "arm GET app");
            match state.apps.get(&app) {
                Some(found) => axum::Json(found.body.clone()).into_response(),
                None => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({"error": {"code": "ResourceNotFound", "message": "no app"}})),
                )
                    .into_response(),
            }
        }
        ("PUT", []) => {
            bump(&mut state, "arm PUT app");
            let mut spec: Value = serde_json::from_slice(&body).unwrap();
            let suffix = spec["properties"]["template"]["revisionSuffix"].as_str().unwrap().to_string();
            let revision = format!("{app}--{suffix}");
            spec["properties"]["latestRevisionName"] = json!(revision);
            spec["properties"]["configuration"]["ingress"]["fqdn"] =
                json!(format!("{app}.internal.fake-env.westeurope.azurecontainerapps.io"));
            let created = format!("2026-11-{:02}T00:00:00Z", 1 + state.apps.get(&app).map_or(0, |a| a.revisions.len()));
            let entry = state.apps.entry(app.clone()).or_default();
            entry.body = spec;
            entry.revisions.push((revision, Revision { active: true, created, replicas: 0 }));
            let operation = format!("op{}", state.operations.len() + 1);
            state.operations.insert(operation.clone(), 1);
            (
                StatusCode::CREATED,
                [("azure-asyncoperation", format!("{base}/operations/{operation}")), ("retry-after", "0".into())],
                axum::Json(json!({})),
            )
                .into_response()
        }
        ("PATCH", []) => {
            bump(&mut state, "arm PATCH app");
            let patch: Value = serde_json::from_slice(&body).unwrap();
            let Some(found) = state.apps.get_mut(&app) else { return StatusCode::NOT_FOUND.into_response() };
            if let Some(traffic) = patch["properties"]["configuration"]["ingress"].get("traffic") {
                found.body["properties"]["configuration"]["ingress"]["traffic"] = traffic.clone();
            }
            (
                StatusCode::ACCEPTED,
                [
                    ("azure-asyncoperation", format!("{base}/operations/patch{}", state.counts["arm PATCH app"])),
                    ("retry-after", "0".into()),
                ],
                "",
            )
                .into_response()
        }
        ("GET", ["revisions"]) => {
            bump(&mut state, "arm GET revisions");
            let Some(found) = state.apps.get(&app) else { return StatusCode::NOT_FOUND.into_response() };
            let value: Vec<Value> = found
                .revisions
                .iter()
                .map(|(name, revision)| json!({"name": name, "properties": {"active": revision.active, "createdTime": revision.created, "replicas": revision.replicas}}))
                .collect();
            axum::Json(json!({"value": value})).into_response()
        }
        ("POST", ["revisions", revision, "deactivate"]) => {
            bump(&mut state, "arm deactivate");
            let Some(found) = state.apps.get_mut(&app) else { return StatusCode::NOT_FOUND.into_response() };
            match found.revisions.iter_mut().find(|(name, _)| name == revision) {
                Some((_, found)) => {
                    found.active = false;
                    found.replicas = 0;
                    StatusCode::OK.into_response()
                }
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        ("GET", ["revisions", revision, "replicas"]) => {
            bump(&mut state, "arm GET replicas");
            let Some(found) = state.apps.get(&app) else { return StatusCode::NOT_FOUND.into_response() };
            let Some((_, found)) = found.revisions.iter().find(|(name, _)| name == revision) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let value: Vec<Value> = (0..found.replicas)
                .map(|index| json!({"name": format!("{revision}-replica-{index}"), "properties": {"runningState": "Running"}}))
                .collect();
            axum::Json(json!({"value": value})).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `<app>---<label>.<domain>`: wake the labelled revision from zero, then answer.
async fn ingress(shared: Arc<Mutex<FakeState>>, host: &str, path: &str) -> Response {
    let (app, label) = {
        let first = host.split('.').next().unwrap_or("");
        let (app, label) = first.split_once("---").unwrap_or(("", ""));
        (app.to_string(), label.to_string())
    };
    let cold_start = {
        let mut state = shared.lock().unwrap();
        bump(&mut state, "probe");
        let revision_name = format!("{app}--{label}");
        let Some(found) = state.apps.get(&app) else { return StatusCode::NOT_FOUND.into_response() };
        let labelled = found.body["properties"]["configuration"]["ingress"]["traffic"]
            .as_array()
            .is_some_and(|traffic| traffic.iter().any(|entry| entry["label"] == label.as_str()));
        let Some((_, revision)) = found.revisions.iter().find(|(name, _)| *name == revision_name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !labelled || !revision.active {
            return StatusCode::NOT_FOUND.into_response();
        }
        if revision.replicas == 0 { state.cold_start } else { Duration::ZERO }
    };
    tokio::time::sleep(cold_start).await;
    {
        let mut state = shared.lock().unwrap();
        let revision_name = format!("{app}--{label}");
        if let Some(found) = state.apps.get_mut(&app)
            && let Some((_, revision)) = found.revisions.iter_mut().find(|(name, _)| *name == revision_name)
            && revision.replicas == 0
        {
            revision.replicas = 1;
            bump(&mut state, "scale from zero");
        }
    }
    if path == "/healthz" { (StatusCode::OK, "ok").into_response() } else { StatusCode::NOT_FOUND.into_response() }
}
