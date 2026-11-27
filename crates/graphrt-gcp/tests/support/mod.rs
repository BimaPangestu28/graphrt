//! Fake Google endpoints for the conformance suite: one scripted server (from
//! `graphrt-provider-api`'s harness) emulating the metadata server, the OAuth
//! token endpoint, the Cloud Run Admin API v2 and Secret Manager, plus an axum
//! "Cloud Run instance" running a fake agent (`/readyz`, streaming runs).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use graphrt_gcp::auth::{GcpAuth, GcpAuthBuilder, ServiceAccountKey};
#[cfg(feature = "provider-cloud-run")]
use graphrt_gcp::cloud_run::CloudRunConfig;
use graphrt_provider_api::conformance::{FakeHandler, FakeResponse, FakeServer, Fault, RecordedRequest};
use graphrt_provider_api::{DeploymentSpec, Resources};
use serde_json::{Value, json};

pub const PROJECT: &str = "demo-project";
pub const REGION: &str = "europe-west1";
pub const TEST_KEY_PEM: &str = include_str!("../fixtures/test-only-rsa2048.pem");
pub const CLIENT_EMAIL: &str = "gateway@demo-project.iam.gserviceaccount.com";

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A key file around the test-only RSA key.
pub fn service_account_json() -> String {
    json!({
        "type": "service_account",
        "project_id": PROJECT,
        "private_key_id": "kid-test-1",
        "private_key": TEST_KEY_PEM,
        "client_email": CLIENT_EMAIL,
        "token_uri": "https://oauth2.googleapis.com/token",
    })
    .to_string()
}

/// An unsigned JWT with these claims (the fake token endpoint's ID tokens).
pub fn fake_jwt(claims: &Value) -> String {
    format!(
        "{}.{}.fake-signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

pub fn jwt_claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

/// Mutable state of the fake Google APIs.
#[derive(Default)]
pub struct GoogleState {
    /// Cloud Run services by resource name.
    pub services: BTreeMap<String, Value>,
    /// Secret Manager secrets: id -> versions.
    pub secrets: BTreeMap<String, Vec<String>>,
    /// The token endpoint's clock minus the real clock, in seconds.
    pub token_clock_offset_secs: i64,
    /// `expires_in` of issued access tokens.
    pub access_token_lifetime_secs: u64,
    /// URL the fake services report (the fake instance).
    pub instance_url: String,
    pub operations: u64,
}

pub struct FakeGoogle {
    /// Admin API + Secret Manager.
    pub server: FakeServer,
    /// Metadata server + OAuth token endpoint.
    pub auth_server: FakeServer,
    pub state: Arc<Mutex<GoogleState>>,
    pub tokens_issued: Arc<AtomicU64>,
}

impl FakeGoogle {
    pub async fn start(instance_url: &str) -> Self {
        let state = Arc::new(Mutex::new(GoogleState {
            access_token_lifetime_secs: 3600,
            instance_url: instance_url.to_owned(),
            ..GoogleState::default()
        }));
        let tokens_issued = Arc::new(AtomicU64::new(0));
        let handler_state = state.clone();
        let handler_tokens = tokens_issued.clone();
        let handler: FakeHandler =
            Arc::new(move |request: &RecordedRequest| handle(&handler_state, &handler_tokens, request));
        let server = FakeServer::start(handler.clone()).await.unwrap();
        let auth_server = FakeServer::start(handler).await.unwrap();
        Self { server, auth_server, state, tokens_issued }
    }

    pub fn url(&self) -> String {
        self.server.base_url()
    }

    #[cfg(feature = "provider-cloud-run")]
    pub fn config(&self) -> CloudRunConfig {
        let mut config = CloudRunConfig::new(PROJECT, REGION);
        config.admin_endpoint = self.url();
        config.secret_manager_endpoint = self.url();
        config.operation_poll_interval = Duration::from_millis(20);
        config.admin_requests_per_second = 1_000.0;
        config.admin_burst = 1_000;
        config
    }

    pub fn auth_url(&self) -> String {
        self.auth_server.base_url()
    }

    pub fn service_account_auth(&self) -> GcpAuth {
        let key = ServiceAccountKey::from_json(&service_account_json()).unwrap();
        GcpAuthBuilder::service_account(key).token_endpoint(format!("{}/token", self.auth_url())).build().unwrap()
    }

    pub fn metadata_auth(&self) -> GcpAuth {
        GcpAuthBuilder::metadata_server(self.auth_url()).build().unwrap()
    }

    pub fn all_requests(&self) -> Vec<RecordedRequest> {
        let mut requests = self.auth_server.requests();
        requests.extend(self.server.requests());
        requests
    }

    pub fn requests_to(&self, method: &str, path_prefix: &str) -> Vec<RecordedRequest> {
        self.all_requests().into_iter().filter(|r| r.method == method && r.path.starts_with(path_prefix)).collect()
    }

    /// Queues a fault on the server that owns `fault.path_prefix`.
    pub fn inject(&self, fault: Fault) {
        if fault.path_prefix.starts_with("/token") || fault.path_prefix.starts_with("/computeMetadata") {
            self.auth_server.inject(fault);
        } else {
            self.server.inject(fault);
        }
    }

    pub fn clear_requests(&self) {
        self.server.clear_requests();
        self.auth_server.clear_requests();
    }

    pub fn service(&self, resource: &str) -> Option<Value> {
        self.state.lock().unwrap().services.get(resource).cloned()
    }

    pub fn edit_service(&self, resource: &str, edit: impl FnOnce(&mut Value)) {
        let mut state = self.state.lock().unwrap();
        edit(state.services.get_mut(resource).unwrap());
    }
}

fn handle(state: &Mutex<GoogleState>, tokens_issued: &AtomicU64, request: &RecordedRequest) -> FakeResponse {
    let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
    let metadata = "/computeMetadata/v1/instance/service-accounts/default/";
    if let Some(kind) = path.strip_prefix(metadata) {
        return metadata_response(state, tokens_issued, request, kind, query);
    }
    if path == "/token" {
        return token_response(state, tokens_issued, request);
    }
    if !request.header("authorization").is_some_and(|value| value.starts_with("Bearer ya29.")) {
        return FakeResponse::json(401, &json!({"error": {"code": 401, "status": "UNAUTHENTICATED"}}));
    }
    if let Some(name) = path.strip_prefix("/v2/") {
        return admin_response(state, request, name);
    }
    if let Some(rest) = path.strip_prefix(&format!("/v1/projects/{PROJECT}/secrets")) {
        return secret_response(state, request, rest, query);
    }
    FakeResponse::status(404)
}

fn issue_access_token(state: &Mutex<GoogleState>, tokens_issued: &AtomicU64) -> Value {
    let number = tokens_issued.fetch_add(1, Ordering::SeqCst) + 1;
    let lifetime = state.lock().unwrap().access_token_lifetime_secs;
    json!({ "access_token": format!("ya29.fake-{number}"), "expires_in": lifetime, "token_type": "Bearer" })
}

fn issue_id_token(tokens_issued: &AtomicU64, audience: &str) -> String {
    let number = tokens_issued.fetch_add(1, Ordering::SeqCst) + 1;
    let now = unix_now();
    fake_jwt(
        &json!({ "aud": audience, "iat": now, "exp": now + 3600, "iss": "https://accounts.google.com", "n": number }),
    )
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(value))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            decoded.push(u8::from_str_radix(&value[index + 1..index + 3], 16).unwrap());
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap()
}

fn metadata_response(
    state: &Mutex<GoogleState>,
    tokens_issued: &AtomicU64,
    request: &RecordedRequest,
    kind: &str,
    query: &str,
) -> FakeResponse {
    if request.header("metadata-flavor") != Some("Google") {
        return FakeResponse::status(403).with_header("Metadata-Flavor", "Google");
    }
    let response = match kind {
        "token" => FakeResponse::json(200, &issue_access_token(state, tokens_issued)),
        "identity" => {
            let audience = query_param(query, "audience").unwrap_or_default();
            FakeResponse {
                status: 200,
                headers: Vec::new(),
                body: issue_id_token(tokens_issued, &audience).into_bytes(),
            }
        }
        _ => FakeResponse::status(404),
    };
    response.with_header("Metadata-Flavor", "Google")
}

fn http_date(unix: i64) -> String {
    // Days-to-civil (Howard Hinnant), enough for an IMF-fixdate.
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month_name =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(month - 1) as usize];
    format!(
        "{weekday}, {day:02} {month_name} {year} {:02}:{:02}:{:02} GMT",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// RFC 7523 JWT bearer grant: checks the RS256 signature with the test key's
/// public half, `iss`, `aud` and that `iat` is within ±60 s of the fake clock.
fn token_response(state: &Mutex<GoogleState>, tokens_issued: &AtomicU64, request: &RecordedRequest) -> FakeResponse {
    let server_now = unix_now() + state.lock().unwrap().token_clock_offset_secs;
    let date = http_date(server_now);
    let form = request.body_text();
    let assertion = form.split('&').find_map(|pair| pair.strip_prefix("assertion=")).unwrap_or_default().to_owned();
    if !form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer") {
        return FakeResponse::json(400, &json!({"error": "unsupported_grant_type"})).with_header("Date", &date);
    }
    let parts: Vec<&str> = assertion.split('.').collect();
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    let public_key = test_public_key();
    let verified = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, public_key)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .is_ok();
    if !verified {
        return FakeResponse::json(
            400,
            &json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}),
        )
        .with_header("Date", &date);
    }
    let header = jwt_claims(&format!("x.{}", parts[0]));
    let claims = jwt_claims(&assertion);
    assert_eq!(header["kid"], "kid-test-1");
    assert_eq!(claims["iss"], CLIENT_EMAIL);
    assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
    let issued_at = claims["iat"].as_i64().unwrap();
    let expires_at = claims["exp"].as_i64().unwrap();
    if (issued_at - server_now).abs() > 60 || expires_at - issued_at > 3600 {
        return FakeResponse::json(
            400,
            &json!({"error": "invalid_grant", "error_description": "Invalid JWT: Token must be a short-lived token (60 minutes) and in a reasonable timeframe. Check your iat and exp values in the JWT claim."}),
        )
        .with_header("Date", &date);
    }
    let body = match claims.get("target_audience").and_then(Value::as_str) {
        Some(audience) => json!({ "id_token": issue_id_token(tokens_issued, audience) }),
        None => {
            assert_eq!(claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
            issue_access_token(state, tokens_issued)
        }
    };
    FakeResponse::json(200, &body).with_header("Date", &date)
}

fn test_public_key() -> Vec<u8> {
    let body: String = TEST_KEY_PEM.lines().filter(|line| !line.starts_with("-----")).collect();
    let der = STANDARD.decode(body).unwrap();
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
    use ring::signature::KeyPair as _;
    pair.public_key().as_ref().to_vec()
}

/// Cloud Run Admin API v2: services.get / services.patch(allowMissing) / operations.get.
fn admin_response(state: &Mutex<GoogleState>, request: &RecordedRequest, name: &str) -> FakeResponse {
    let mut state = state.lock().unwrap();
    if name.contains("/operations/") {
        let service_name = name.split("/operations/").nth(1).unwrap().replace('~', "/");
        let service = state.services.get(&service_name).cloned().unwrap_or(Value::Null);
        return FakeResponse::json(200, &json!({ "name": name, "done": true, "response": service }));
    }
    match request.method.as_str() {
        "GET" => match state.services.get(name) {
            Some(service) => FakeResponse::json(200, service),
            None => FakeResponse::json(404, &json!({"error": {"code": 404, "status": "NOT_FOUND"}})),
        },
        "PATCH" => {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            // Like the real Admin API: "traffic[0].tag: length must be between 3 and 47 characters".
            let short_tag =
                body["traffic"].as_array().into_iter().flatten().filter_map(|target| target["tag"].as_str());
            if let Some(tag) = short_tag.into_iter().find(|tag| !(3..=47).contains(&tag.len())) {
                let message = format!(
                    "Violation in UpdateServiceRequest.service.traffic.tag `{tag}`: length must be between 3 and 47 characters."
                );
                return FakeResponse::json(
                    400,
                    &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": message}}),
                );
            }
            let instance_url = state.instance_url.clone();
            let existing = state.services.get(name).cloned();
            let traffic_only = request.path.contains("updateMask=traffic");
            let mut service = match (existing, traffic_only) {
                (Some(mut existing), true) => {
                    existing["traffic"] = body["traffic"].clone();
                    existing
                }
                _ => body,
            };
            materialise_status(&mut service, name, &instance_url);
            state.services.insert(name.to_owned(), service);
            state.operations += 1;
            let operation = format!("projects/{PROJECT}/locations/{REGION}/operations/{}", name.replace('/', "~"));
            FakeResponse::json(200, &json!({ "name": operation, "done": false }))
        }
        _ => FakeResponse::status(405),
    }
}

/// What Cloud Run fills in after a successful rollout.
fn materialise_status(service: &mut Value, name: &str, instance_url: &str) {
    let revision = service["template"]["revision"].as_str().unwrap_or("rev").to_owned();
    service["name"] = json!(name);
    service["uri"] = json!(instance_url);
    service["latestReadyRevision"] = json!(format!("{name}/revisions/{revision}"));
    service["terminalCondition"] = json!({ "type": "Ready", "state": "CONDITION_SUCCEEDED" });
    let statuses: Vec<Value> = service["traffic"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|target| {
            target["tag"].as_str().map(|tag| json!({ "tag": tag, "uri": instance_url, "percent": target["percent"] }))
        })
        .collect();
    service["trafficStatuses"] = json!(statuses);
}

fn secret_response(state: &Mutex<GoogleState>, request: &RecordedRequest, rest: &str, query: &str) -> FakeResponse {
    let mut state = state.lock().unwrap();
    if rest.is_empty() && request.method == "POST" {
        let id = query_param(query, "secretId").unwrap();
        if state.secrets.contains_key(&id) {
            return FakeResponse::json(
                409,
                &json!({"error": {"code": 409, "status": "ALREADY_EXISTS", "message": "Secret already exists"}}),
            );
        }
        state.secrets.insert(id.clone(), Vec::new());
        return FakeResponse::json(200, &json!({ "name": format!("projects/123456/secrets/{id}") }));
    }
    if let Some(id) = rest.strip_prefix('/').and_then(|rest| rest.strip_suffix(":addVersion")) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let value = String::from_utf8(STANDARD.decode(body["payload"]["data"].as_str().unwrap()).unwrap()).unwrap();
        let Some(versions) = state.secrets.get_mut(id) else { return FakeResponse::status(404) };
        versions.push(value);
        let number = versions.len();
        return FakeResponse::json(200, &json!({ "name": format!("projects/123456/secrets/{id}/versions/{number}") }));
    }
    FakeResponse::status(404)
}

/// A fake Cloud Run instance running the agent.
pub struct FakeInstance {
    pub url: String,
    pub state: Arc<InstanceState>,
    task: tokio::task::JoinHandle<()>,
}

pub struct InstanceState {
    pub healthz_calls: AtomicU32,
    /// Delay of the first `/readyz` wake probe (cold start), in milliseconds.
    pub cold_start_ms: AtomicU64,
    /// Status the `/readyz` wake probe answers (200 by default; 429 = no instance available).
    pub healthz_status: AtomicU32,
    /// Audience every invoker token must carry.
    pub expected_audience: Mutex<Option<String>>,
    pub rejected_tokens: AtomicU32,
}

impl Drop for FakeInstance {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeInstance {
    pub async fn start() -> Self {
        let state = Arc::new(InstanceState {
            healthz_calls: AtomicU32::new(0),
            cold_start_ms: AtomicU64::new(0),
            healthz_status: AtomicU32::new(200),
            expected_audience: Mutex::new(None),
            rejected_tokens: AtomicU32::new(0),
        });
        let app = Router::new()
            .route("/readyz", get(healthz))
            // Cloud Run's front end answers `/healthz` itself, before the container.
            .route("/healthz", get(|| async { StatusCode::NOT_FOUND }))
            .route("/internal/v1/runs", post(stream_run))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }
}

fn token_ok(state: &InstanceState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.expected_audience.lock().unwrap().clone() else { return true };
    let token = headers
        .get("x-serverless-authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let ok = token.is_some_and(|token| jwt_claims(token)["aud"] == expected.as_str());
    if !ok {
        state.rejected_tokens.fetch_add(1, Ordering::SeqCst);
    }
    ok
}

async fn healthz(State(state): State<Arc<InstanceState>>, headers: HeaderMap) -> impl IntoResponse {
    if !token_ok(&state, &headers) {
        return (StatusCode::FORBIDDEN, "forbidden");
    }
    let call = state.healthz_calls.fetch_add(1, Ordering::SeqCst);
    if call == 0 {
        tokio::time::sleep(Duration::from_millis(state.cold_start_ms.load(Ordering::SeqCst))).await;
    }
    let status = StatusCode::from_u16(state.healthz_status.load(Ordering::SeqCst) as u16).unwrap();
    (status, "ok")
}

/// Streams `chunks` SSE events, `interval_ms` apart, each carrying its send time
/// in microseconds since the Unix epoch, padded to ~`size` bytes.
async fn stream_run(
    State(state): State<Arc<InstanceState>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    if !token_ok(&state, &headers) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let request: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let chunks = request["chunks"].as_u64().unwrap_or(10);
    let interval = Duration::from_millis(request["interval_ms"].as_u64().unwrap_or(10));
    let size = request["size"].as_u64().unwrap_or(512) as usize;
    let stream = futures::stream::unfold(0u64, move |index| async move {
        if index >= chunks {
            return None;
        }
        if index > 0 {
            tokio::time::sleep(interval).await;
        }
        let sent_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        let mut event = format!("id: {index}\ndata: {{\"sent_us\":{sent_us},\"pad\":\"");
        event.push_str(&"x".repeat(size.saturating_sub(event.len() + 5)));
        event.push_str("\"}\n\n");
        Some((Ok::<_, std::convert::Infallible>(event), index + 1))
    });
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// A Cloud Run-shaped deployment spec.
pub fn spec(name: &str, version: &str) -> DeploymentSpec {
    DeploymentSpec {
        name: name.to_owned(),
        version: version.to_owned(),
        image: Some("europe-docker.pkg.dev/demo-project/graphrt/agent:0.5.0".to_owned()),
        command: vec!["graphrt".into(), "serve".into(), "--trust-gateway".into(), "agent.yaml".into()],
        env: BTreeMap::from([("RUST_LOG".to_owned(), "info".to_owned())]),
        secrets: vec![graphrt_provider_api::SecretRef {
            env: "OPENAI_API_KEY".to_owned(),
            reference: format!("projects/{PROJECT}/secrets/openai-key/versions/3"),
        }],
        max_active_runs: 8,
        min_replicas: 0,
        max_replicas: 4,
        grace_secs: 120,
        resources: Resources { cpu_millis: 1000, memory_mib: 512 },
        workflow_fingerprint: "sha256:abc123".to_owned(),
        provider_settings: BTreeMap::from([(
            "service_account".to_owned(),
            json!(format!("agent@{PROJECT}.iam.gserviceaccount.com")),
        )]),
    }
}

pub fn service_resource(name: &str) -> String {
    format!("projects/{PROJECT}/locations/{REGION}/services/graphrt-{name}")
}
