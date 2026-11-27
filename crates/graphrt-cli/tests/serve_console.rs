//! The admin console of `graphrt serve --console`: a real server and a fake
//! OpenID Connect provider. People sign in, get the role their groups give,
//! are held to it on the server's own routes, and what they change (secrets,
//! versions, roles) applies after the console restarts the server.
#![cfg(all(feature = "console", unix))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value as Json_, json};
use sha2::{Digest, Sha256};

const API_KEY: &str = "automation-key";
const CLIENT_SECRET: &str = "client-secret";
const CONSOLE_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-console-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A sign in the fake provider will complete: the claims of the ID token.
#[derive(Clone)]
struct Grant {
    claims: Json_,
    /// Sign with a key the provider does not publish.
    forged: bool,
}

struct Idp {
    issuer: String,
    key: EcdsaKeyPair,
    other_key: EcdsaKeyPair,
    grants: Mutex<HashMap<String, Grant>>,
    /// `code_challenge` of each code, checked against the verifier at the token endpoint.
    challenges: Mutex<HashMap<String, String>>,
    token_requests: Mutex<Vec<HashMap<String, String>>>,
}

fn key_pair() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
}

impl Idp {
    fn sign(&self, claims: &Json_, forged: bool) -> String {
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "ES256", "kid": "k1", "typ": "JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{payload}");
        let key = if forged { &self.other_key } else { &self.key };
        let signature = key.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

async fn start_idp() -> Arc<Idp> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let idp = Arc::new(Idp {
        issuer,
        key: key_pair(),
        other_key: key_pair(),
        grants: Mutex::new(HashMap::new()),
        challenges: Mutex::new(HashMap::new()),
        token_requests: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(Arc::clone(&idp));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Json_> {
    Json(json!({
        "issuer": idp.issuer,
        "authorization_endpoint": format!("{}/authorize", idp.issuer),
        "token_endpoint": format!("{}/token", idp.issuer),
        "jwks_uri": format!("{}/jwks", idp.issuer),
        "end_session_endpoint": format!("{}/logout", idp.issuer),
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Json<Json_> {
    let point = idp.key.public_key().as_ref();
    Json(json!({"keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    }]}))
}

async fn token(State(idp): State<Arc<Idp>>, body: Bytes) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let url = reqwest::Url::parse(&format!("x:?{}", String::from_utf8_lossy(&body))).unwrap();
    let form: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    idp.token_requests.lock().unwrap().push(form.clone());
    let code = form.get("code").cloned().unwrap_or_default();
    let challenge = idp.challenges.lock().unwrap().remove(&code);
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let pkce_ok = challenge.is_some_and(|c| c == URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())));
    let grant = idp.grants.lock().unwrap().remove(&code);
    match grant {
        Some(grant) if pkce_ok && form.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET) => Json(
            json!({"access_token": "a", "token_type": "Bearer", "id_token": idp.sign(&grant.claims, grant.forged)}),
        )
        .into_response(),
        _ => (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response(),
    }
}

// ---- a minimal HTTP/1.1 client --------------------------------------------------------

#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn cookies(&self) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).map(|(_, v)| v.as_str()).collect()
    }
    fn json(&self) -> Json_ {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| json!({"raw": self.text()}))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn request(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let Ok(mut stream) = std::net::TcpStream::connect(addr) else {
        return Answer { status: 0, headers: Vec::new(), body: Vec::new() };
    };
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body)).is_err() {
        return Answer { status: 0, headers: Vec::new(), body: Vec::new() };
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut rest = raw.get(split + 4..).unwrap_or_default().to_vec();
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked")) {
        rest = dechunk(&rest);
    }
    Answer { status, headers, body: rest }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(end) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&raw[..end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&raw[end + 2..end + 2 + size]);
        raw = &raw[end + 2 + size + 2..];
    }
    out
}

/// The value of `name` in a `Set-Cookie` header list.
fn cookie_value(cookies: &[&str], name: &str) -> Option<String> {
    cookies
        .iter()
        .find_map(|c| c.strip_prefix(&format!("{name}=")))
        .map(|rest| rest.split(';').next().unwrap_or("").to_owned())
}

fn query_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
        .unwrap_or_default()
}

// ---- the server ----------------------------------------------------------------------

struct Server {
    addr: String,
    child: std::process::Child,
    log: PathBuf,
}

impl Server {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

fn workflow(answer: &str) -> String {
    format!(
        "version: \"1\"\nname: hello\nenv:\n  - {{ name: GREETING, type: string }}\ngraph:\n  nodes:\n    - {{ id: start, type: start }}\n    - {{ id: out, type: answer, answer: \"{{{{#env.GREETING#}}}} {answer}\" }}\n  edges:\n    - {{ source: start, target: out }}\n"
    )
}

async fn start_server(dir: &Path, idp: &Idp) -> Server {
    let addr = free_addr();
    let flows = dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(flows.join("hello.yaml"), workflow("from disk")).unwrap();
    let config = dir.join("console.yaml");
    std::fs::write(
        &config,
        format!(
            "public_url: http://{addr}\nenvironments: [dev, prod]\nenvironment: dev\ndata_dir: {}\n\
             oidc:\n  issuer: {}\n  client_id: graphrt\n  client_secret_env: GRAPHRT_TEST_OIDC_SECRET\n\
             roles:\n  owners: [owner@example.com]\n  groups: {{ops: operator, staff: viewer}}\n",
            dir.join("console").display(),
            idp.issuer
        ),
    )
    .unwrap();
    let log = dir.join("serve.log");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", flows.to_str().unwrap(), "--addr", &addr, "--store", dir.join("store").to_str().unwrap()])
        .args(["--console", config.to_str().unwrap(), "--no-default-plugins", "--drain-timeout", "2s"])
        .env("GRAPHRT_API_KEY", API_KEY)
        .env("GRAPHRT_CONSOLE_KEY", CONSOLE_KEY)
        .env("GRAPHRT_TEST_OIDC_SECRET", CLIENT_SECRET)
        .env_remove("GREETING")
        .env_remove("GRAPHRT_CONSOLE")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn graphrt serve");
    let server = Server { addr, child, log };
    wait_healthy(&server).await;
    server
}

async fn wait_healthy(server: &Server) {
    for _ in 0..1500 {
        if request(&server.addr, "GET", "/healthz", &[], b"").status == 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("graphrt serve did not become healthy:\n{}", server.log());
}

/// A signed in browser.
struct Browser {
    cookie: String,
    csrf: String,
    subject: String,
}

/// Sign in through the fake provider with these claims (plus the right `iss`,
/// `aud`, `exp` and `nonce`).
fn sign_in(server: &Server, idp: &Idp, claims: Json_) -> Result<Browser, Answer> {
    let login = request(&server.addr, "GET", "/console/login?return_to=/monitor/", &[], b"");
    assert_eq!(login.status, 303, "{}", login.text());
    let location = login.header("location").unwrap().to_owned();
    assert!(location.starts_with(&format!("{}/authorize?", idp.issuer)), "{location}");
    assert_eq!(query_param(&location, "code_challenge_method"), "S256");
    let login_cookie = cookie_value(&login.cookies(), "graphrt-login").unwrap();
    let (state, nonce) = (query_param(&location, "state"), query_param(&location, "nonce"));
    let mut claims = claims;
    let subject = claims["sub"].as_str().unwrap().to_owned();
    claims["iss"] = idp.issuer.clone().into();
    claims["aud"] = "graphrt".into();
    claims["exp"] = (graphrt_core::store::now_ms() / 1000 + 300).into();
    claims["nonce"] = nonce.into();
    let forged = claims.get("forged").is_some();
    let code = format!("code-{subject}-{}", graphrt_core::store::now_ms());
    idp.grants.lock().unwrap().insert(code.clone(), Grant { claims, forged });
    idp.challenges.lock().unwrap().insert(code.clone(), query_param(&location, "code_challenge"));
    let cookie = format!("graphrt-login={login_cookie}");
    let path = format!("/console/callback?code={code}&state={state}");
    let callback = request(&server.addr, "GET", &path, &[("Cookie", &cookie)], b"");
    if callback.status != 303 {
        return Err(callback);
    }
    assert_eq!(callback.header("location"), Some("/monitor/"));
    let set = callback.cookies();
    let session = cookie_value(&set, "graphrt-session").unwrap();
    let session_cookie = set.iter().find(|c| c.starts_with("graphrt-session=")).unwrap();
    assert!(session_cookie.contains("HttpOnly") && session_cookie.contains("SameSite=Strict"), "{session_cookie}");
    let cookie = format!("graphrt-session={session}");
    let me = request(&server.addr, "GET", "/v1/console/session", &[("Cookie", &cookie)], b"").json();
    assert_eq!(me["signed_in"], true, "{me}");
    Ok(Browser { cookie, csrf: me["csrf_token"].as_str().unwrap().to_owned(), subject })
}

impl Browser {
    fn get(&self, server: &Server, path: &str) -> Answer {
        request(&server.addr, "GET", path, &[("Cookie", &self.cookie)], b"")
    }

    fn send(&self, server: &Server, method: &str, path: &str, body: &[u8]) -> Answer {
        let origin = format!("http://{}", server.addr);
        let headers = [
            ("Cookie", self.cookie.as_str()),
            ("X-Graphrt-CSRF", self.csrf.as_str()),
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
        ];
        request(&server.addr, method, path, &headers, body)
    }
}

fn with_key(server: &Server, method: &str, path: &str, body: &[u8]) -> Answer {
    let auth = format!("Bearer {API_KEY}");
    request(&server.addr, method, path, &[("Authorization", &auth), ("Content-Type", "application/json")], body)
}

fn answer_of(run: &Answer) -> String {
    let body = run.json();
    body["outputs"]["answer"].as_str().map(str::to_owned).unwrap_or_else(|| panic!("no answer: {body}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn people_sign_in_with_roles_and_owners_apply_secrets_and_versions() {
    let dir = TempDir::new("flow");
    let idp = start_idp().await;
    let server = start_server(&dir.0, &idp).await;
    let addr = server.addr.clone();

    // Nobody is signed in: the console says so, and its API and the server's are closed.
    assert_eq!(request(&addr, "GET", "/v1/console/session", &[], b"").json()["signed_in"], false);
    assert_eq!(request(&addr, "GET", "/v1/console/workflows", &[], b"").status, 401);
    assert_eq!(request(&addr, "GET", "/v1/runs", &[], b"").status, 401);
    // The API key keeps working for automation, on the console API too.
    let listed = with_key(&server, "GET", "/v1/console/workflows", b"");
    assert_eq!(listed.status, 200, "{}", listed.text());
    let served = &listed.json()["served"][0];
    assert_eq!(served["name"], "hello");
    assert_eq!(served["env"], json!(["GREETING"]));
    assert_eq!(served["hash"].as_str().unwrap().len(), 64);

    // A viewer reads, and is refused what changes something.
    let viewer = sign_in(&server, &idp, json!({"sub": "v1", "email": "v@example.com", "groups": ["staff"]})).unwrap();
    assert_eq!(viewer.get(&server, "/v1/runs").status, 200);
    assert_eq!(viewer.get(&server, "/v1/console/workflows").status, 200);
    let refused = viewer.send(&server, "POST", "/v1/workflows/hello/runs", b"{}");
    assert_eq!(refused.status, 403, "{}", refused.text());
    assert_eq!(viewer.get(&server, "/v1/console/secrets").status, 403);
    // Webhook deliveries are for the systems that send them, not for browsers.
    assert_eq!(viewer.send(&server, "POST", "/v1/hooks/anything", b"{}").status, 403);
    // DNS rebinding: a signed in request must name the console's host.
    let rebound = request(&addr, "GET", "/v1/runs", &[("Cookie", &viewer.cookie), ("Host", "evil.example")], b"");
    assert_eq!(rebound.status, 403);

    // Someone without a role cannot sign in at all.
    let outsider = sign_in(&server, &idp, json!({"sub": "x1", "email": "x@example.com", "groups": ["other"]}));
    assert_eq!(outsider.err().map(|a| a.status), Some(403));
    // A token signed with another key is refused.
    let forged = sign_in(&server, &idp, json!({"sub": "o9", "email": "owner@example.com", "forged": true}));
    assert_eq!(forged.err().map(|a| a.status), Some(502));

    let owner = sign_in(&server, &idp, json!({"sub": "o1", "email": "owner@example.com"})).unwrap();
    // A change needs the session's CSRF token.
    let no_csrf =
        request(&addr, "PUT", "/v1/console/secrets/GREETING", &[("Cookie", &owner.cookie)], b"{\"value\":\"x\"}");
    assert_eq!(no_csrf.status, 403);
    let cross_site = request(
        &addr,
        "PUT",
        "/v1/console/secrets/GREETING",
        &[("Cookie", &owner.cookie), ("X-Graphrt-CSRF", &owner.csrf), ("Origin", "https://evil.example")],
        b"{\"value\":\"x\"}",
    );
    assert_eq!(cross_site.status, 403);

    // Secrets are set, listed by name only, and refused for server variables.
    let set = owner.send(&server, "PUT", "/v1/console/secrets/GREETING", b"{\"value\":\"Hi from the console\"}");
    assert_eq!(set.status, 201, "{}", set.text());
    let secrets = owner.get(&server, "/v1/console/secrets");
    assert!(!secrets.text().contains("Hi from"), "{}", secrets.text());
    assert_eq!(secrets.json()["secrets"][0]["used_by"], json!(["hello"]));
    assert_eq!(owner.send(&server, "PUT", "/v1/console/secrets/GRAPHRT_API_KEY", b"{\"value\":\"x\"}").status, 400);

    // A version that does not compile is refused; a good one goes to `dev`, this server's environment.
    let broken =
        owner.send(&server, "POST", "/v1/console/workflows/hello/versions", b"graph: {nodes: [{id: a, type: nope}]}");
    assert_eq!(broken.status, 422, "{}", broken.text());
    let uploaded =
        owner.send(&server, "POST", "/v1/console/workflows/hello/versions?note=v2", workflow("v2").as_bytes());
    assert_eq!(uploaded.status, 201, "{}", uploaded.text());
    assert_eq!(uploaded.json()["restart_needed"], true);
    let pending = owner.get(&server, "/v1/console/workflows").json()["pending"].clone();
    assert_eq!(pending, json!(["secrets", "workflow:hello"]));

    // The owner makes the viewer an operator: it applies at once.
    let path = format!("/v1/console/members/{}", viewer.subject);
    assert_eq!(owner.send(&server, "PUT", &path, b"{\"role\":\"operator\"}").status, 200);
    let started = viewer.send(&server, "POST", "/v1/workflows/hello/runs", b"{\"inputs\":{}}");
    assert_eq!(started.status, 200, "{}", started.text());
    assert_eq!(answer_of(&started).trim(), "from disk");

    // Apply: the server drains and starts again with the secret and version 1 of the registry.
    let applied = owner.send(&server, "POST", "/v1/console/apply", b"");
    assert_eq!(applied.status, 202, "{}", applied.text());
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_healthy(&server).await;
    let mut answer = String::new();
    for _ in 0..200 {
        let run = with_key(&server, "POST", "/v1/workflows/hello/runs", b"{\"inputs\":{}}");
        if run.status == 200 {
            answer = answer_of(&run);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(answer, "Hi from the console v2", "{}", server.log());
    // Sessions outlive the restart; nothing is pending any more.
    assert_eq!(owner.get(&server, "/v1/console/workflows").json()["pending"], json!([]));
    let registry = owner.get(&server, "/v1/console/workflows").json();
    assert_eq!(registry["served"][0]["version"], 1);

    // Promote dev to prod, then roll prod back to nothing earlier: refused.
    let promoted = owner.send(&server, "POST", "/v1/console/workflows/hello/promote", b"{\"from\":\"dev\"}");
    assert_eq!(promoted.status, 200, "{}", promoted.text());
    assert_eq!(promoted.json()["environment"], "prod");
    assert_eq!(promoted.json()["restart_needed"], false);
    let rollback = owner.send(&server, "POST", "/v1/console/workflows/hello/rollback", b"{\"environment\":\"prod\"}");
    assert_eq!(rollback.status, 409);

    // The audit log has it all, without the secret's value, and its chain holds.
    let audit = owner.get(&server, "/v1/console/audit?limit=200").json();
    let actions: Vec<&str> =
        audit["entries"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    for expected in [
        "auth.sign_in",
        "secret.set",
        "registry.upload",
        "member.role",
        "run.start",
        "server.restart",
        "registry.promote",
    ] {
        assert!(actions.contains(&expected), "{expected} missing from {actions:?}");
    }
    let denied = owner.get(&server, "/v1/console/audit?outcome=denied").json();
    assert!(denied["entries"].as_array().unwrap().len() >= 2, "{denied}");
    let export = owner.get(&server, "/v1/console/audit/export");
    assert_eq!(export.status, 200);
    let lines: Vec<Json_> = export.text().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.len() >= actions.len());
    assert!(!export.text().contains("Hi from the console"));
    assert_eq!(owner.get(&server, "/v1/console/audit/verify").json()["broken_at_seq"], Json_::Null);
    assert_eq!(viewer.get(&server, "/v1/console/audit").status, 403);

    // Signing out ends the session.
    assert_eq!(owner.send(&server, "POST", "/v1/console/sign-out", b"").status, 200);
    assert_eq!(owner.get(&server, "/v1/console/workflows").status, 401);
    // The client secret went to the token endpoint, never anywhere else.
    assert!(
        idp.token_requests.lock().unwrap().iter().all(|r| r.get("client_id").map(String::as_str) == Some("graphrt"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callback_from_another_browser_is_refused() {
    let dir = TempDir::new("binding");
    let idp = start_idp().await;
    let server = start_server(&dir.0, &idp).await;
    let login = request(&server.addr, "GET", "/console/login", &[], b"");
    let location = login.header("location").unwrap().to_owned();
    let state = query_param(&location, "state");
    let path = format!("/console/callback?code=whatever&state={state}");
    let other = request(&server.addr, "GET", &path, &[("Cookie", "graphrt-login=someone-else")], b"");
    assert_eq!(other.status, 400, "{}", other.text());
    // The state is used up either way.
    let login_cookie = format!("graphrt-login={}", cookie_value(&login.cookies(), "graphrt-login").unwrap());
    assert_eq!(request(&server.addr, "GET", &path, &[("Cookie", &login_cookie)], b"").status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_needs_an_api_key() {
    let dir = TempDir::new("nokey");
    std::fs::write(dir.0.join("hello.yaml"), workflow("x")).unwrap();
    std::fs::write(
        dir.0.join("console.yaml"),
        "public_url: http://127.0.0.1:1\ndata_dir: console\noidc: {issuer: http://127.0.0.1:2, client_id: x}\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", dir.0.join("hello.yaml").to_str().unwrap(), "--addr", "127.0.0.1:0"])
        .args(["--console", dir.0.join("console.yaml").to_str().unwrap(), "--no-default-plugins"])
        .env_remove("GRAPHRT_API_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--console needs an API key"));
}
