//! Fake AWS endpoints for the conformance tests: ECS (JSON 1.1), Lambda
//! `InvokeWithResponseStream`, STS, IMDSv2 and the container credential endpoint.
//!
//! The fake verifies every SigV4 signature with the secret it handed out, enforces
//! the ±5 min clock window, and can inject throttling, 5xx, expired tokens and
//! capacity failures.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::result_large_err
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{any, get, post, put};
use serde_json::{Value, json};

use graphrt_provider_aws::auth::sigv4::{self, AwsCredentials, SigningRequest};

pub const ACCESS_KEY: &str = "AKIDFAKE";
pub const SECRET_KEY: &str = "fake-secret-key-never-logged";
pub const SESSION_TOKEN: &str = "fake-session-token-never-logged";

/// What the Lambda fake does on the next invoke.
#[derive(Clone, Debug)]
pub enum LambdaBehavior {
    /// Streaming prelude, then `chunks` SSE events every `interval`, then InvokeComplete.
    Stream { chunks: usize, interval: Duration },
    /// Wait `delay`, then reply with one buffered envelope holding `chunks` events.
    Buffered { chunks: usize, delay: Duration },
    /// Prelude, then all events in one chunk after `delay` (an agent that does not flush).
    SingleChunk { chunks: usize, delay: Duration },
    /// Stream `chunks` events, then InvokeComplete with `Sandbox.Timedout`.
    Timeout { chunks: usize },
    /// Prelude, one event, then stall for `stall` (gateway cap test).
    Stall { stall: Duration },
    /// InvokeComplete with a function error.
    FunctionError,
}

#[derive(Clone, Debug)]
pub struct FakeTask {
    pub arn: String,
    pub started_by: String,
    pub task_definition: String,
    pub created: Instant,
    pub ip: String,
    pub stopped: bool,
    pub stopped_reason: Option<String>,
}

#[derive(Debug, Default)]
pub struct FakeState {
    pub tasks: Vec<FakeTask>,
    pub next_task: u32,
    /// RUNNING this long after RunTask.
    pub provisioning: Duration,
    pub client_tokens: HashMap<String, Vec<String>>,
    pub calls: HashMap<String, u32>,
    pub throttle_next: u32,
    pub server_error_next: u32,
    pub expired_token_next: u32,
    pub capacity_failure: bool,
    /// Start the tasks of the next RunTask but answer 500 (a lost response).
    pub lose_next_run_task_response: bool,
    /// Seconds the fake's clock is ahead of the real one.
    pub server_clock_offset: i64,
    pub signature_failures: u32,
    pub stop_reasons: Vec<(String, String)>,
    pub lambda: Option<LambdaBehavior>,
    pub lambda_requests: Vec<Value>,
    pub lambda_queries: Vec<String>,
    /// Credentials served by the credential endpoints (`cred_hits` counts fetches).
    pub cred_hits: u32,
    pub cred_expiration: Option<String>,
    pub imds_token_seen: bool,
    pub sts_forms: Vec<String>,
    pub container_auth_seen: Vec<String>,
    /// RegisterTaskDefinition requests, in order.
    pub task_definitions: Vec<Value>,
    pub functions: HashMap<String, FakeFunction>,
    /// Polls a create / update stays `InProgress`.
    pub update_polls: u32,
    /// The next CreateFunction answers 409 (created concurrently by someone else).
    pub create_conflict_next: bool,
    /// Lambda control-plane calls (`METHOD path`), in order.
    pub lambda_calls: Vec<String>,
    /// SecretString / parameter values by ARN.
    pub secret_values: HashMap<String, String>,
}

/// A Lambda function in the fake control plane.
#[derive(Clone, Debug, Default)]
pub struct FakeFunction {
    pub configuration: Value,
    pub image: String,
    /// (version, image, configuration) of each published version.
    pub versions: Vec<(String, String, Value)>,
    pub aliases: HashMap<String, String>,
    pub url: Option<Value>,
    pub polls_left: u32,
}

pub type Shared = Arc<Mutex<FakeState>>;

pub struct FakeAws {
    pub base_url: String,
    pub state: Shared,
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn http_date(unix: i64) -> String {
    // Civil-from-days, as in sigv4::amz_date.
    let amz = sigv4::amz_date(unix as u64);
    let (y, m, d) = (&amz[0..4], &amz[4..6], &amz[6..8]);
    let (hh, mm, ss) = (&amz[9..11], &amz[11..13], &amz[13..15]);
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        [m.parse::<usize>().unwrap() - 1];
    format!("Mon, {d} {month} {y} {hh}:{mm}:{ss} GMT")
}

fn json_response(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn aws_error(status: StatusCode, code: &str, message: &str, server_unix: i64) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/x-amz-json-1.1")
        .header("date", http_date(server_unix))
        .body(Body::from(json!({"__type": format!("com.amazonaws.ecs#{code}"), "message": message}).to_string()))
        .unwrap()
}

/// Verify SigV4 and the clock window. `Err` is the response to send.
fn verify(
    state: &Shared,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    service: &str,
) -> Result<(), Response> {
    let server_now = now_unix() + state.lock().unwrap().server_clock_offset;
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let authorization = header("authorization");
    let date = header("x-amz-date");
    let signed_headers =
        authorization.split("SignedHeaders=").nth(1).and_then(|s| s.split(',').next()).unwrap_or_default().to_owned();
    let date_unix = graphrt_provider_aws::auth::parse_iso8601(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        date.get(0..4).unwrap_or("0"),
        date.get(4..6).unwrap_or("0"),
        date.get(6..8).unwrap_or("0"),
        date.get(9..11).unwrap_or("0"),
        date.get(11..13).unwrap_or("0"),
        date.get(13..15).unwrap_or("0")
    ))
    .map(|d| d as i64)
    .unwrap_or(0);
    if (date_unix - server_now).abs() > 300 {
        return Err(aws_error(
            StatusCode::FORBIDDEN,
            "InvalidSignatureException",
            &format!("Signature expired: {date} is now earlier than the allowed window"),
            server_now,
        ));
    }
    let values: Vec<(String, String)> = signed_headers.split(';').map(|name| (name.to_owned(), header(name))).collect();
    let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let raw_query = uri.query().unwrap_or_default();
    let decoded: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> = decoded.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let creds = AwsCredentials {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET_KEY.into(),
        session_token: Some(SESSION_TOKEN.into()),
    };
    let expected = sigv4::authorization(
        &creds,
        &SigningRequest {
            method: method.as_str(),
            path: uri.path(),
            query: &query,
            headers: &pairs,
            payload_hash: &sigv4::sha256_hex(body),
            region: "eu-west-1",
            service,
            amz_date: &date,
            double_encode_path: true,
        },
    );
    if expected != authorization || header("x-amz-security-token") != SESSION_TOKEN {
        state.lock().unwrap().signature_failures += 1;
        return Err(aws_error(StatusCode::FORBIDDEN, "InvalidSignatureException", "signature mismatch", server_now));
    }
    Ok(())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Injected failures shared by every signed API. `Some` = respond with it.
fn injected_failure(state: &Shared) -> Option<Response> {
    let mut s = state.lock().unwrap();
    let server_now = now_unix() + s.server_clock_offset;
    if s.expired_token_next > 0 {
        s.expired_token_next -= 1;
        return Some(aws_error(
            StatusCode::FORBIDDEN,
            "ExpiredTokenException",
            "The security token included in the request is expired",
            server_now,
        ));
    }
    if s.throttle_next > 0 {
        s.throttle_next -= 1;
        return Some(aws_error(StatusCode::BAD_REQUEST, "ThrottlingException", "Rate exceeded", server_now));
    }
    if s.server_error_next > 0 {
        s.server_error_next -= 1;
        return Some(aws_error(StatusCode::INTERNAL_SERVER_ERROR, "ServerException", "internal", server_now));
    }
    None
}

fn task_json(task: &FakeTask, provisioning: Duration) -> Value {
    let running = task.created.elapsed() >= provisioning;
    let (last, desired) = if task.stopped {
        ("STOPPED", "STOPPED")
    } else if running {
        ("RUNNING", "RUNNING")
    } else {
        ("PROVISIONING", "RUNNING")
    };
    let mut details = vec![json!({"name": "subnetId", "value": "subnet-1"})];
    if running || task.stopped {
        details.push(json!({"name": "privateIPv4Address", "value": task.ip}));
    }
    json!({
        "taskArn": task.arn,
        "lastStatus": last,
        "desiredStatus": desired,
        "startedBy": task.started_by,
        "taskDefinitionArn": task.task_definition,
        "stoppedReason": task.stopped_reason,
        "attachments": [{"type": "ElasticNetworkInterface", "details": details}],
    })
}

async fn ecs_or_sts(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        return sts(&state, &body);
    }
    let target = headers.get("x-amz-target").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let service = if target.starts_with("secretsmanager.") {
        "secretsmanager"
    } else if target.starts_with("AmazonSSM.") {
        "ssm"
    } else {
        "ecs"
    };
    if let Err(response) = verify(&state, &method, &uri, &headers, &body, service) {
        return response;
    }
    if let Some(response) = injected_failure(&state) {
        return response;
    }
    if service != "ecs" {
        return secret_value(&state, &target, &body);
    }
    let operation = target.rsplit('.').next().unwrap_or_default().to_owned();
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut s = state.lock().unwrap();
    *s.calls.entry(operation.clone()).or_default() += 1;
    let provisioning = s.provisioning;
    match operation.as_str() {
        "RegisterTaskDefinition" => {
            let family = request["family"].as_str().unwrap_or_default().to_owned();
            s.task_definitions.push(request.clone());
            let revision = s.task_definitions.iter().filter(|t| t["family"] == family.as_str()).count();
            let arn = format!("arn:aws:ecs:eu-west-1:123456789012:task-definition/{family}:{revision}");
            json_response(StatusCode::OK, json!({"taskDefinition": {"taskDefinitionArn": arn, "revision": revision}}))
        }
        "RunTask" => {
            if s.capacity_failure {
                return json_response(
                    StatusCode::OK,
                    json!({"tasks": [], "failures": [
                    {"reason": "Capacity is unavailable at this time. Please try again later or in a different availability zone"}]}),
                );
            }
            let token = request["clientToken"].as_str().unwrap_or_default().to_owned();
            if let Some(arns) = s.client_tokens.get(&token).cloned() {
                let tasks: Vec<Value> =
                    s.tasks.iter().filter(|t| arns.contains(&t.arn)).map(|t| task_json(t, provisioning)).collect();
                return json_response(StatusCode::OK, json!({"tasks": tasks, "failures": []}));
            }
            let count = request["count"].as_u64().unwrap_or(1);
            assert!(count <= 10, "RunTask count above 10");
            assert!(request["startedBy"].as_str().unwrap().len() <= 36);
            let mut created = Vec::new();
            for _ in 0..count {
                s.next_task += 1;
                let n = s.next_task;
                let task = FakeTask {
                    arn: format!("arn:aws:ecs:eu-west-1:123:task/c/{n:08}"),
                    started_by: request["startedBy"].as_str().unwrap_or_default().to_owned(),
                    task_definition: request["taskDefinition"].as_str().unwrap_or_default().to_owned(),
                    created: Instant::now(),
                    ip: format!("10.0.{}.{}", n / 250, n % 250 + 1),
                    stopped: false,
                    stopped_reason: None,
                };
                created.push(task.clone());
                s.tasks.push(task);
            }
            s.client_tokens.insert(token, created.iter().map(|t| t.arn.clone()).collect());
            if s.lose_next_run_task_response {
                s.lose_next_run_task_response = false;
                return aws_error(StatusCode::INTERNAL_SERVER_ERROR, "ServerException", "lost", now_unix());
            }
            let tasks: Vec<Value> = created.iter().map(|t| task_json(t, provisioning)).collect();
            json_response(StatusCode::OK, json!({"tasks": tasks, "failures": []}))
        }
        "DescribeTasks" => {
            let wanted: Vec<&str> = request["tasks"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            assert!(wanted.len() <= 100);
            let tasks: Vec<Value> = s
                .tasks
                .iter()
                .filter(|t| wanted.contains(&t.arn.as_str()))
                .map(|t| task_json(t, provisioning))
                .collect();
            json_response(StatusCode::OK, json!({"tasks": tasks, "failures": []}))
        }
        "ListTasks" => {
            let started_by = request["startedBy"].as_str().unwrap_or_default();
            let arns: Vec<&str> =
                s.tasks.iter().filter(|t| !t.stopped && t.started_by == started_by).map(|t| t.arn.as_str()).collect();
            json_response(StatusCode::OK, json!({"taskArns": arns}))
        }
        "StopTask" => {
            let arn = request["task"].as_str().unwrap_or_default().to_owned();
            let reason = request["reason"].as_str().unwrap_or_default().to_owned();
            match s.tasks.iter_mut().find(|t| t.arn == arn) {
                Some(task) => {
                    task.stopped = true;
                    task.stopped_reason = Some(reason.clone());
                    s.stop_reasons.push((arn, reason));
                    json_response(StatusCode::OK, json!({"task": {"taskArn": request["task"]}}))
                }
                None => aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidParameterException",
                    "The referenced task was not found.",
                    now_unix(),
                ),
            }
        }
        other => aws_error(StatusCode::BAD_REQUEST, "UnknownOperationException", other, now_unix()),
    }
}

fn secret_value(state: &Shared, target: &str, body: &[u8]) -> Response {
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let s = state.lock().unwrap();
    if target == "AmazonSSM.GetParameter" {
        assert_eq!(request["WithDecryption"], true);
        let name = request["Name"].as_str().unwrap_or_default();
        return match s.secret_values.get(name) {
            Some(value) => json_response(StatusCode::OK, json!({"Parameter": {"Name": name, "Value": value}})),
            None => aws_error(StatusCode::BAD_REQUEST, "ParameterNotFound", "no such parameter", now_unix()),
        };
    }
    let id = request["SecretId"].as_str().unwrap_or_default();
    match s.secret_values.get(id) {
        Some(value) => json_response(StatusCode::OK, json!({"ARN": id, "SecretString": value})),
        None => aws_error(StatusCode::BAD_REQUEST, "ResourceNotFoundException", "no such secret", now_unix()),
    }
}

fn lambda_error(status: StatusCode, code: &str, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-amzn-errortype", code)
        .body(Body::from(json!({"Type": "User", "Message": message}).to_string()))
        .unwrap()
}

fn function_arn(name: &str) -> String {
    if name.starts_with("arn:") {
        name.to_owned()
    } else {
        format!("arn:aws:lambda:eu-west-1:123456789012:function:{name}")
    }
}

fn function_configuration(name: &str, function: &FakeFunction) -> Value {
    let mut configuration = function.configuration.clone();
    let in_progress = function.polls_left > 0;
    configuration["FunctionName"] = json!(name);
    configuration["FunctionArn"] = json!(function_arn(name));
    configuration["State"] = json!("Active");
    configuration["LastUpdateStatus"] = json!(if in_progress { "InProgress" } else { "Successful" });
    configuration
}

/// The Lambda control plane (`/2015-03-31/functions…`, `/2021-10-31/functions/{f}/url`).
async fn lambda_control(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = verify(&state, &method, &uri, &headers, &body, "lambda") {
        return response;
    }
    if let Some(response) = injected_failure(&state) {
        return response;
    }
    let path = percent_decode(uri.path());
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut s = state.lock().unwrap();
    s.lambda_calls.push(format!("{method} {path}"));
    let polls = s.update_polls;
    if let Some(name) = path.strip_prefix("/2021-10-31/functions/").and_then(|p| p.strip_suffix("/url")) {
        let qualifier = uri.query().unwrap_or_default().strip_prefix("Qualifier=").unwrap_or_default().to_owned();
        let Some(function) = s.functions.get_mut(name) else {
            return lambda_error(StatusCode::NOT_FOUND, "ResourceNotFoundException", "no function");
        };
        if !function.aliases.contains_key(&qualifier) {
            return lambda_error(StatusCode::NOT_FOUND, "ResourceNotFoundException", "no alias");
        }
        if method == Method::POST && function.url.is_some() {
            return lambda_error(StatusCode::CONFLICT, "ResourceConflictException", "url exists");
        }
        let url = json!({"FunctionUrl": "https://abc.lambda-url.eu-west-1.on.aws/", "AuthType": request["AuthType"],
                         "InvokeMode": request["InvokeMode"], "Qualifier": qualifier});
        function.url = Some(url.clone());
        return json_response(StatusCode::OK, url);
    }
    let rest = path.strip_prefix("/2015-03-31/functions").unwrap_or_default().to_owned();
    if rest.is_empty() && method == Method::POST {
        let name = request["FunctionName"].as_str().unwrap_or_default().to_owned();
        if s.create_conflict_next {
            s.create_conflict_next = false;
            s.functions.insert(name.clone(), FakeFunction { configuration: json!({}), ..Default::default() });
            return lambda_error(StatusCode::CONFLICT, "ResourceConflictException", "Function already exist");
        }
        if s.functions.contains_key(&name) {
            return lambda_error(StatusCode::CONFLICT, "ResourceConflictException", "Function already exist");
        }
        assert_eq!(request["PackageType"], "Image");
        let image = request["Code"]["ImageUri"].as_str().unwrap_or_default().to_owned();
        let function = FakeFunction { configuration: request.clone(), image, polls_left: polls, ..Default::default() };
        let configuration = function_configuration(&name, &function);
        s.functions.insert(name, function);
        return json_response(StatusCode::CREATED, configuration);
    }
    let rest = rest.trim_start_matches('/');
    let (name, suffix) = match rest.find("/") {
        Some(at) => (rest[..at].to_owned(), rest[at..].to_owned()),
        None => (rest.to_owned(), String::new()),
    };
    let Some(function) = s.functions.get_mut(&name) else {
        return lambda_error(StatusCode::NOT_FOUND, "ResourceNotFoundException", "Function not found");
    };
    match (method.as_str(), suffix.as_str()) {
        ("GET", "") => json_response(StatusCode::OK, json!({"Configuration": function_configuration(&name, function)})),
        ("GET", "/configuration") => {
            function.polls_left = function.polls_left.saturating_sub(1);
            let configuration = function_configuration(&name, function);
            json_response(StatusCode::OK, configuration)
        }
        ("PUT", "/configuration") | ("PUT", "/code") if function.polls_left > 0 => lambda_error(
            StatusCode::CONFLICT,
            "ResourceConflictException",
            "The operation cannot be performed at this time. An update is in progress",
        ),
        ("PUT", "/configuration") => {
            if let (Some(target), Some(update)) = (function.configuration.as_object_mut(), request.as_object()) {
                for (k, v) in update {
                    target.insert(k.clone(), v.clone());
                }
            }
            function.polls_left = polls;
            json_response(StatusCode::OK, function_configuration(&name, function))
        }
        ("PUT", "/code") => {
            function.image = request["ImageUri"].as_str().unwrap_or_default().to_owned();
            function.polls_left = polls;
            json_response(StatusCode::OK, function_configuration(&name, function))
        }
        ("POST", "/versions") => {
            if function.polls_left > 0 {
                return lambda_error(StatusCode::CONFLICT, "ResourceConflictException", "update in progress");
            }
            let mut configuration = function.configuration.clone();
            configuration.as_object_mut().map(|c| c.remove("Description"));
            let version = match function.versions.last() {
                Some((v, image, c)) if *image == function.image && *c == configuration => v.clone(),
                _ => {
                    let v = (function.versions.len() + 1).to_string();
                    function.versions.push((v.clone(), function.image.clone(), configuration));
                    v
                }
            };
            json_response(
                StatusCode::CREATED,
                json!({"Version": version, "FunctionArn": format!("{}:{version}", function_arn(&name))}),
            )
        }
        ("POST", "/aliases") => {
            let alias = request["Name"].as_str().unwrap_or_default().to_owned();
            if function.aliases.contains_key(&alias) {
                return lambda_error(StatusCode::CONFLICT, "ResourceConflictException", "alias exists");
            }
            function.aliases.insert(alias.clone(), request["FunctionVersion"].as_str().unwrap_or_default().to_owned());
            json_response(StatusCode::CREATED, json!({"Name": alias}))
        }
        ("GET", alias_path) if alias_path.starts_with("/aliases/") => {
            let alias = &alias_path["/aliases/".len()..];
            match function.aliases.get(alias) {
                Some(version) => json_response(StatusCode::OK, json!({"Name": alias, "FunctionVersion": version})),
                None => lambda_error(StatusCode::NOT_FOUND, "ResourceNotFoundException", "alias not found"),
            }
        }
        other => lambda_error(StatusCode::BAD_REQUEST, "InvalidRequest", &format!("unexpected {other:?}")),
    }
}

fn credential_expiration(s: &FakeState) -> String {
    s.cred_expiration.clone().unwrap_or_else(|| "2099-01-01T00:00:00Z".into())
}

fn sts(state: &Shared, body: &[u8]) -> Response {
    let form = String::from_utf8_lossy(body).into_owned();
    let mut s = state.lock().unwrap();
    s.cred_hits += 1;
    s.sts_forms.push(form.clone());
    if !form.contains("WebIdentityToken=web-identity-jwt") {
        return Response::builder()
            .status(400)
            .body(Body::from(
                "<ErrorResponse><Error><Type>Sender</Type><Code>InvalidIdentityToken</Code><Message>bad token</Message></Error></ErrorResponse>",
            ))
            .unwrap();
    }
    let xml = format!(
        "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
         <AccessKeyId>{ACCESS_KEY}</AccessKeyId><SecretAccessKey>{SECRET_KEY}</SecretAccessKey>\
         <SessionToken>{SESSION_TOKEN}</SessionToken><Expiration>{}</Expiration></Credentials>\
         </AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>",
        credential_expiration(&s)
    );
    Response::builder().status(200).header("content-type", "text/xml").body(Body::from(xml)).unwrap()
}

fn credentials_json(s: &FakeState) -> Value {
    json!({"Code": "Success", "AccessKeyId": ACCESS_KEY, "SecretAccessKey": SECRET_KEY, "Token": SESSION_TOKEN,
           "Expiration": credential_expiration(s)})
}

async fn container_credentials(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let mut s = state.lock().unwrap();
    s.cred_hits += 1;
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        s.container_auth_seen.push(auth.to_owned());
    }
    json_response(StatusCode::OK, credentials_json(&s))
}

async fn imds_token(State(state): State<Shared>, headers: HeaderMap) -> Response {
    assert!(headers.contains_key("x-aws-ec2-metadata-token-ttl-seconds"));
    state.lock().unwrap().imds_token_seen = true;
    Response::new(Body::from("imds-session-token"))
}

fn imds_authorized(headers: &HeaderMap) -> bool {
    headers.get("x-aws-ec2-metadata-token").and_then(|v| v.to_str().ok()) == Some("imds-session-token")
}

async fn imds_roles(headers: HeaderMap) -> Response {
    if !imds_authorized(&headers) {
        return Response::builder().status(401).body(Body::empty()).unwrap();
    }
    Response::new(Body::from("graphrt-gateway-role\n"))
}

async fn imds_role(State(state): State<Shared>, Path(role): Path<String>, headers: HeaderMap) -> Response {
    if !imds_authorized(&headers) || role != "graphrt-gateway-role" {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    }
    let mut s = state.lock().unwrap();
    s.cred_hits += 1;
    json_response(StatusCode::OK, credentials_json(&s))
}

fn frame(event_type: &str, payload: &[u8]) -> Bytes {
    Bytes::from(graphrt_provider_aws::lambda::eventstream::encode(
        &[(":event-type", event_type), (":message-type", "event"), (":content-type", "application/octet-stream")],
        payload,
    ))
}

fn prelude() -> Bytes {
    let mut p = br#"{"statusCode":200,"headers":{"content-type":"text/event-stream"}}"#.to_vec();
    p.extend_from_slice(&[0u8; 8]);
    frame("PayloadChunk", &p)
}

/// One SSE event of ≈ 200 bytes carrying its send time in µs since the Unix epoch.
pub fn sse_event(index: usize) -> Vec<u8> {
    let micros = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
    format!("id: {index}\nevent: token\ndata: {{\"sent_us\":{micros},\"pad\":\"{}\"}}\n\n", "x".repeat(120))
        .into_bytes()
}

fn complete(error: Option<(&str, &str)>) -> Bytes {
    let body = match error {
        Some((code, details)) => json!({"ErrorCode": code, "ErrorDetails": details, "LogResult": ""}),
        None => json!({}),
    };
    frame("InvokeComplete", body.to_string().as_bytes())
}

async fn lambda_invoke(
    State(state): State<Shared>,
    Path(function): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = verify(&state, &method, &uri, &headers, &body, "lambda") {
        return response;
    }
    if let Some(response) = injected_failure(&state) {
        return response;
    }
    assert_eq!(headers.get("x-amz-invocation-type").unwrap(), "RequestResponse");
    let behavior = {
        let mut s = state.lock().unwrap();
        s.lambda_requests.push(serde_json::from_slice(&body).unwrap());
        s.lambda_queries.push(format!("{function}?{}", uri.query().unwrap_or_default()));
        s.lambda.clone().unwrap_or(LambdaBehavior::Stream { chunks: 3, interval: Duration::from_millis(5) })
    };
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let send = |b: Bytes| {
            let sender = sender.clone();
            async move { sender.send(Ok(b)).await.is_ok() }
        };
        match behavior {
            LambdaBehavior::Stream { chunks, interval } => {
                send(prelude()).await;
                for i in 0..chunks {
                    tokio::time::sleep(interval).await;
                    if !send(frame("PayloadChunk", &sse_event(i))).await {
                        return;
                    }
                }
                send(complete(None)).await;
            }
            LambdaBehavior::Buffered { chunks, delay } => {
                tokio::time::sleep(delay).await;
                let body: Vec<u8> = (0..chunks).flat_map(sse_event).collect();
                let envelope = json!({"statusCode": 200, "headers": {"content-type": "text/event-stream"},
                    "body": String::from_utf8(body).unwrap(), "isBase64Encoded": false});
                send(frame("PayloadChunk", envelope.to_string().as_bytes())).await;
                send(complete(None)).await;
            }
            LambdaBehavior::SingleChunk { chunks, delay } => {
                send(prelude()).await;
                tokio::time::sleep(delay).await;
                let body: Vec<u8> = (0..chunks).flat_map(sse_event).collect();
                send(frame("PayloadChunk", &body)).await;
                send(complete(None)).await;
            }
            LambdaBehavior::Timeout { chunks } => {
                send(prelude()).await;
                for i in 0..chunks {
                    send(frame("PayloadChunk", &sse_event(i))).await;
                }
                send(complete(Some(("Sandbox.Timedout", "Task timed out after 900.00 seconds")))).await;
            }
            LambdaBehavior::Stall { stall } => {
                send(prelude()).await;
                send(frame("PayloadChunk", &sse_event(0))).await;
                tokio::time::sleep(stall).await;
                send(complete(None)).await;
            }
            LambdaBehavior::FunctionError => {
                send(prelude()).await;
                send(complete(Some(("Runtime.ExitError", "exit status 1")))).await;
            }
        }
    });
    let stream = futures::stream::unfold(receiver, |mut r| async move { r.recv().await.map(|item| (item, r)) });
    Response::builder()
        .status(200)
        .header("content-type", "application/vnd.amazon.eventstream")
        .header("x-amz-executed-version", "7")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Start the fake on a random local port.
pub async fn start() -> FakeAws {
    let state: Shared = Arc::new(Mutex::new(FakeState::default()));
    let app = Router::new()
        .route("/", post(ecs_or_sts))
        .route("/2021-11-15/functions/{function}/response-streaming-invocations", post(lambda_invoke))
        .route("/2015-03-31/functions", post(lambda_control))
        .route("/2015-03-31/functions/{*rest}", any(lambda_control))
        .route("/2021-10-31/functions/{*rest}", any(lambda_control))
        .route("/creds", get(container_credentials))
        .route("/latest/api/token", put(imds_token))
        .route("/latest/meta-data/iam/security-credentials/", get(imds_roles))
        .route("/latest/meta-data/iam/security-credentials/{role}", get(imds_role))
        .fallback(any(|| async { StatusCode::NOT_FOUND }))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeAws { base_url: format!("http://{address}"), state }
}

impl FakeAws {
    /// A chain that fetches credentials from the fake container endpoint.
    pub fn chain(&self) -> Arc<graphrt_provider_aws::auth::CredentialChain> {
        let config = graphrt_provider_aws::auth::ChainConfig {
            container_relative_uri: Some("/creds".into()),
            container_host: Some(self.base_url.clone()),
            region: "eu-west-1".into(),
            imds_disabled: true,
            ..Default::default()
        };
        Arc::new(graphrt_provider_aws::auth::CredentialChain::new(config, Default::default()).unwrap())
    }

    pub fn endpoint(&self, service: &str) -> graphrt_provider_aws::rest::ServiceEndpoint {
        graphrt_provider_aws::rest::ServiceEndpoint {
            base_url: self.base_url.clone(),
            region: "eu-west-1".into(),
            service: service.into(),
        }
    }

    pub fn calls(&self, operation: &str) -> u32 {
        self.state.lock().unwrap().calls.get(operation).copied().unwrap_or(0)
    }

    pub fn live_tasks(&self) -> usize {
        self.state.lock().unwrap().tasks.iter().filter(|t| !t.stopped).count()
    }
}

/// Fast retry policy for tests.
pub fn fast_retry() -> graphrt_provider_aws::rest::ApiLimits {
    graphrt_provider_aws::rest::ApiLimits {
        retry: graphrt_provider_aws::api::http::RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(40),
        },
        rate_per_sec: 10_000.0,
        burst: 10_000,
    }
}

/// The HTTP URL of an endpoint address.
pub fn url_of(address: &graphrt_provider_aws::api::EndpointAddress) -> String {
    match address {
        graphrt_provider_aws::api::EndpointAddress::Http(url) => url.clone(),
        other => panic!("unexpected address {other:?}"),
    }
}

/// p-quantile (0..=1) of durations.
pub fn quantile(mut samples: Vec<Duration>, q: f64) -> Duration {
    samples.sort();
    let index = ((samples.len() as f64 - 1.0) * q).round() as usize;
    samples[index.min(samples.len() - 1)]
}
