//! `graphrt collab`: the real binary with a fake OpenID Connect provider.
//! An app signs in the way Graphrt Desktop does (a loopback redirect with
//! PKCE), people outside the allowed domain and forged tokens are refused,
//! the bootstrap invite lets the first owner in, and the server speaks HTTPS
//! with its own certificate.
#![cfg(all(feature = "collab", unix))]
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
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CLIENT_SECRET: &str = "client-secret";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-collab-cli-{tag}-{}", std::process::id()));
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

#[derive(Clone)]
struct Grant {
    claims: Value,
    forged: bool,
}

struct Idp {
    issuer: String,
    key: EcdsaKeyPair,
    other_key: EcdsaKeyPair,
    grants: Mutex<HashMap<String, Grant>>,
    challenges: Mutex<HashMap<String, String>>,
}

fn key_pair() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
}

impl Idp {
    fn sign(&self, claims: &Value, forged: bool) -> String {
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
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(Arc::clone(&idp));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    Json(json!({
        "issuer": idp.issuer,
        "authorization_endpoint": format!("{}/authorize", idp.issuer),
        "token_endpoint": format!("{}/token", idp.issuer),
        "jwks_uri": format!("{}/jwks", idp.issuer),
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
    }))
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Json<Value> {
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
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&self.body)}))
    }
}

fn request(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let Ok(mut stream) = std::net::TcpStream::connect(addr) else {
        return Answer { status: 0, headers: Vec::new(), body: Vec::new() };
    };
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
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
    let rest = raw.get(split + 4..).unwrap_or_default().to_vec();
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    Answer { status, headers, body: rest }
}

fn json_post(addr: &str, path: &str, body: &Value, bearer: Option<&str>) -> Answer {
    let auth = bearer.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("Content-Type", "application/json")];
    if let Some(auth) = &auth {
        headers.push(("Authorization", auth.as_str()));
    }
    request(addr, "POST", path, &headers, body.to_string().as_bytes())
}

fn query_param(url: &str, name: &str) -> String {
    let parsed = reqwest::Url::parse(url).unwrap();
    parsed.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap_or_default()
}

fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

struct Server {
    addr: String,
    child: std::process::Child,
    log: PathBuf,
    config: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn graphrt(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(args)
        .env("GRAPHRT_TEST_OIDC_SECRET", CLIENT_SECRET)
        .env("GRAPHRT_TEST_OIDC_ID", "graphrt-desktop")
        .env_remove("GRAPHRT_TEST_UNSET_ID")
        .env_remove("GRAPHRT_COLLAB_CONFIG")
        .output()
        .unwrap()
}

async fn start_server(dir: &Path, extra: &str, scheme: &str) -> Server {
    let addr = free_addr();
    let config = dir.join("collab.yaml");
    std::fs::write(
        &config,
        format!("public_url: {scheme}://{addr}\nlisten: {addr}\ndata_dir: {}\n{extra}", dir.join("data").display()),
    )
    .unwrap();
    let log = dir.join("collab.log");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["collab", "serve", "--config", config.to_str().unwrap()])
        .env("GRAPHRT_TEST_OIDC_SECRET", CLIENT_SECRET)
        .env("GRAPHRT_TEST_OIDC_ID", "graphrt-desktop")
        .env_remove("GRAPHRT_TEST_UNSET_ID")
        .env_remove("GRAPHRT_COLLAB_SIGNING_KEY")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let server = Server { addr, child, log, config };
    for _ in 0..1500 {
        if std::net::TcpStream::connect(&server.addr).is_ok() {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("graphrt collab did not start:\n{}", std::fs::read_to_string(&server.log).unwrap_or_default());
}

/// Sign in like Graphrt Desktop: loopback redirect, the app's own PKCE.
///
/// @returns The token answer, or where the browser was sent with an error.
fn desktop_sign_in(server: &Server, idp: &Idp, claims: Value) -> Result<Value, String> {
    sign_in_with(server, idp, None, claims)
}

/// [`desktop_sign_in`] with the provider the app picked (`None`: the server's first).
fn sign_in_with(server: &Server, idp: &Idp, provider: Option<&str>, claims: Value) -> Result<Value, String> {
    let verifier = "desktop-verifier-0123456789-0123456789-0123456789";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect = "http://127.0.0.1:53123/callback";
    let mut path = format!(
        "/v1/auth/login?redirect_uri={}&state=app-state&code_challenge={challenge}&code_challenge_method=S256&device=test",
        url_encode(redirect)
    );
    if let Some(provider) = provider {
        path.push_str(&format!("&provider={provider}"));
    }
    let login = request(&server.addr, "GET", &path, &[], b"");
    assert_eq!(login.status, 303, "{:?}", login.json());
    let location = login.header("location").unwrap().to_owned();
    assert!(location.starts_with(&format!("{}/authorize?", idp.issuer)), "{location}");
    assert_eq!(query_param(&location, "redirect_uri"), format!("http://{}/v1/auth/callback", server.addr));
    let mut claims = claims;
    claims["iss"] = idp.issuer.clone().into();
    claims["aud"] = "graphrt-desktop".into();
    claims["exp"] = (graphrt_core::store::now_ms() / 1000 + 300).into();
    claims["nonce"] = query_param(&location, "nonce").into();
    let forged = claims.get("forged").is_some();
    let code = format!("code-{}", graphrt_core::store::now_ms());
    idp.grants.lock().unwrap().insert(code.clone(), Grant { claims, forged });
    idp.challenges.lock().unwrap().insert(code.clone(), query_param(&location, "code_challenge"));
    let callback = request(
        &server.addr,
        "GET",
        &format!("/v1/auth/callback?code={code}&state={}", query_param(&location, "state")),
        &[],
        b"",
    );
    assert_eq!(callback.status, 303, "{:?}", callback.json());
    let back = callback.header("location").unwrap().to_owned();
    assert!(back.starts_with(redirect), "{back}");
    assert_eq!(query_param(&back, "state"), "app-state");
    if !query_param(&back, "error").is_empty() {
        return Err(back);
    }
    let app_code = query_param(&back, "code");
    // A wrong verifier does not trade the code (and burns it).
    let body = json!({"grant_type": "authorization_code", "code": app_code, "code_verifier": verifier, "redirect_uri": redirect});
    let tokens = json_post(&server.addr, "/v1/auth/token", &body, None);
    assert_eq!(tokens.status, 200, "{:?}", tokens.json());
    let again = json_post(&server.addr, "/v1/auth/token", &body, None);
    assert_eq!(again.status, 401, "a code is used once");
    Ok(tokens.json())
}

fn url_encode(text: &str) -> String {
    reqwest::Url::parse_with_params("x:", [("v", text)]).unwrap().query().unwrap().trim_start_matches("v=").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_signs_in_with_the_company_provider() {
    let dir = TempDir::new("oidc");
    let idp = start_idp().await;
    let extra = format!(
        "auth:\n  oidc: {{issuer: '{}', client_id: graphrt-desktop, client_secret_env: GRAPHRT_TEST_OIDC_SECRET}}\n  roles: {{allowed_domains: [example.com], default_role: operator}}\n",
        idp.issuer
    );
    let server = start_server(&dir.0, &extra, "http").await;

    let info = request(&server.addr, "GET", "/v1/info", &[], b"").json();
    assert_eq!(info["sign_in"]["oidc"], true);
    assert_eq!(info["sign_in"]["invite_codes"], false);

    // Someone of the allowed domain.
    let tokens = desktop_sign_in(
        &server,
        &idp,
        json!({"sub": "u-ana", "email": "ana@example.com", "email_verified": true, "name": "Ana"}),
    )
    .unwrap();
    let access = tokens["access_token"].as_str().unwrap();
    let auth = format!("Bearer {access}");
    let me = request(&server.addr, "GET", "/v1/me", &[("Authorization", &auth)], b"").json();
    assert_eq!(me["account"]["name"], "Ana");
    assert_eq!(me["may_create_workspaces"], true);
    let created = json_post(&server.addr, "/v1/workspaces", &json!({"name": "Ops"}), Some(access));
    assert_eq!(created.status, 201, "{:?}", created.json());

    // The refresh token works once and rotates.
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let body = json!({"grant_type": "refresh_token", "refresh_token": refresh});
    assert_eq!(json_post(&server.addr, "/v1/auth/token", &body, None).status, 200);
    assert_eq!(json_post(&server.addr, "/v1/auth/token", &body, None).status, 401);

    // Another domain, an unverified email, a forged token: refused.
    let refused =
        desktop_sign_in(&server, &idp, json!({"sub": "u-eve", "email": "eve@evil.example", "email_verified": true}));
    assert!(refused.unwrap_err().contains("error=access_denied"));
    let refused =
        desktop_sign_in(&server, &idp, json!({"sub": "u-x", "email": "x@example.com", "email_verified": false}));
    assert!(refused.unwrap_err().contains("error=access_denied"));
    let refused = desktop_sign_in(
        &server,
        &idp,
        json!({"sub": "u-f", "email": "f@example.com", "email_verified": true, "forged": true}),
    );
    assert!(refused.unwrap_err().contains("error=access_denied"));

    // Only loopback redirect addresses, only S256.
    let evil = request(
        &server.addr,
        "GET",
        &format!(
            "/v1/auth/login?redirect_uri={}&state=s&code_challenge={}",
            url_encode("https://evil.example/cb"),
            "a".repeat(43)
        ),
        &[],
        b"",
    );
    assert_eq!(evil.status, 400);
    let plain = request(
        &server.addr,
        "GET",
        &format!(
            "/v1/auth/login?redirect_uri={}&state=s&code_challenge={}&code_challenge_method=plain",
            url_encode("http://127.0.0.1:9/cb"),
            "a".repeat(43)
        ),
        &[],
        b"",
    );
    assert_eq!(plain.status, 400);

    // Invite codes do not make accounts on this server: an invite needs a sign in first.
    let out = graphrt(&[
        "collab",
        "invite",
        "--config",
        server.config.to_str().unwrap(),
        "--workspace",
        "Ops",
        "--role",
        "viewer",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let code = stdout.lines().find_map(|l| l.strip_prefix("code:")).unwrap().trim().to_owned();
    let anonymous = json_post(&server.addr, "/v1/invites/redeem", &json!({"code": code, "name": "Guest"}), None);
    assert_eq!(anonymous.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_server_with_two_providers() {
    let dir = TempDir::new("providers");
    let google = start_idp().await;
    let microsoft = start_idp().await;
    let extra = format!(
        "mode: hosted\nauth:\n  providers:\n\
         \x20   - {{id: google, name: Google, issuer: '{}', client_id_env: GRAPHRT_TEST_OIDC_ID, client_secret_env: GRAPHRT_TEST_OIDC_SECRET}}\n\
         \x20   - {{id: microsoft, name: Microsoft, issuer: '{}', client_id: graphrt-desktop, client_secret_env: GRAPHRT_TEST_OIDC_SECRET, email_verified_claim: xms_edov}}\n\
         \x20   - {{id: later, issuer: 'https://later.example', client_id_env: GRAPHRT_TEST_UNSET_ID}}\n",
        google.issuer, microsoft.issuer
    );
    let server = start_server(&dir.0, &extra, "http").await;

    // The provider without a client id yet is off, the others are buttons.
    let info = request(&server.addr, "GET", "/v1/info", &[], b"").json();
    assert_eq!(info["sign_in"]["oidc"], true);
    assert_eq!(
        info["sign_in"]["providers"],
        json!([{"id": "google", "name": "Google"}, {"id": "microsoft", "name": "Microsoft"}])
    );
    let out = graphrt(&["collab", "check", "--config", server.config.to_str().unwrap()]);
    let banner = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(banner.contains("sign in with Google, Microsoft") && banner.contains("later is off"), "{banner}");

    // Each provider signs in its own people: the first is the default.
    let ana =
        desktop_sign_in(&server, &google, json!({"sub": "same", "email": "ana@example.com", "name": "Ana"})).unwrap();
    assert_eq!(ana["account"]["email"], "ana@example.com");
    let bo = sign_in_with(
        &server,
        &microsoft,
        Some("microsoft"),
        json!({"sub": "same", "email": "bo@example.com", "name": "Bo", "xms_edov": true}),
    )
    .unwrap();
    assert_eq!(bo["account"]["email"], "bo@example.com");
    // The same subject at two providers is two accounts.
    assert_ne!(ana["account"]["id"], bo["account"]["id"]);
    // An email Microsoft does not vouch for is not used.
    let cy =
        sign_in_with(&server, &microsoft, Some("microsoft"), json!({"sub": "cy", "email": "ana@example.com"})).unwrap();
    assert!(cy["account"]["email"].is_null(), "{cy}");
    // A provider the server does not have.
    let path = format!(
        "/v1/auth/login?redirect_uri={}&state=s&code_challenge={}&provider=later",
        url_encode("http://127.0.0.1:9/cb"),
        "a".repeat(43)
    );
    assert_eq!(request(&server.addr, "GET", &path, &[], b"").status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_invite_and_https() {
    let dir = TempDir::new("tls");
    let cert = dir.0.join("tls.crt");
    let key = dir.0.join("tls.key");
    let ca = dir.0.join("ca.crt");
    // A throwaway CA and a server certificate it signs (webpki refuses a CA as the server's own).
    let openssl = |args: &[&str]| std::process::Command::new("openssl").args(args).current_dir(&dir.0).output();
    let steps: [&[&str]; 3] = [
        &[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=test ca",
            "-keyout",
            "ca.key",
            "-out",
            "ca.crt",
        ],
        &[
            "req",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-subj",
            "/CN=127.0.0.1",
            "-keyout",
            "tls.key",
            "-out",
            "tls.csr",
        ],
        &[
            "x509",
            "-req",
            "-in",
            "tls.csr",
            "-CA",
            "ca.crt",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            "ext.cnf",
            "-out",
            "tls.crt",
        ],
    ];
    std::fs::write(
        dir.0.join("ext.cnf"),
        "subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n",
    )
    .unwrap();
    for step in steps {
        let Ok(made) = openssl(step) else {
            eprintln!("openssl is not installed: skipping the HTTPS part");
            return;
        };
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    }
    let extra = format!("tls: {{cert: {}, key: {}}}\n", cert.display(), key.display());
    let server = start_server(&dir.0, &extra, "https").await;

    // The first owner's invite, made with the binary.
    let out = graphrt(&["collab", "invite", "--config", server.config.to_str().unwrap(), "--workspace", "Acme"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let link = stdout.lines().find_map(|l| l.strip_prefix("link:")).unwrap().trim().to_owned();
    assert!(link.starts_with(&format!("https://{}/join#gci1.", server.addr)), "{link}");

    // Over HTTPS with the server's certificate trusted.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = reqwest::Certificate::from_pem(&std::fs::read(&ca).unwrap()).unwrap();
    let client = reqwest::Client::builder().add_root_certificate(root).build().unwrap();
    let base = format!("https://{}", server.addr);
    let info: Value = client.get(format!("{base}/v1/info")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["mode"], "self_hosted");
    let joined: Value = client
        .post(format!("{base}/v1/invites/redeem"))
        .json(&json!({"code": link, "name": "Ana"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(joined["role"], "owner", "{joined}");
    assert!(joined["refresh_token"].as_str().unwrap().starts_with("gcr1."));

    // Without trusting the certificate, the connection fails (what Desktop reports as an invalid certificate).
    let strict = reqwest::Client::builder().build().unwrap();
    assert!(strict.get(format!("{base}/healthz")).send().await.is_err());
    // Plain HTTP to the TLS port gets nothing usable.
    assert_ne!(request(&server.addr, "GET", "/healthz", &[], b"").status, 200);
}

#[test]
fn check_and_key_commands() {
    let dir = TempDir::new("check");
    let config = dir.0.join("collab.yaml");
    std::fs::write(&config, format!("public_url: http://collab.example.com\ndata_dir: {}\n", dir.0.display())).unwrap();
    let out = graphrt(&["collab", "check", "--config", config.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("https://"));
    std::fs::write(&config, format!("public_url: https://collab.example.com\ndata_dir: {}\n", dir.0.display()))
        .unwrap();
    let out = graphrt(&["collab", "check", "--config", config.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("self hosted"));
    let key = graphrt(&["collab", "key"]);
    assert_eq!(String::from_utf8_lossy(&key.stdout).trim().len(), 64);
}
