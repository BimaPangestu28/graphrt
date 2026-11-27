//! `--workspace-api`: the agents' workspace over HTTP (`/v1/files`). Uploads
//! and downloads go through the same workspace as the `files` tools, a client
//! never chooses where on the server a file lands, a user only reaches their
//! own directory, and nothing a client stored is ever served as a page.
#![cfg(all(feature = "files", feature = "jwt"))]
// `WorkspaceOptions` has more fields with the `image` feature.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::needless_update)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_cli::args::WorkspaceApi;
use graphrt_cli::header_inputs::HeaderInputSpec;
use graphrt_cli::serve::{build_state, discover, router_with_uploads};
use graphrt_cli::setup::{ServiceOptions, WorkspaceOptions};
use graphrt_cli::user_auth::UserAuth;
use graphrt_cli::user_auth::token::{ClaimRules, VerifyKey};
use graphrt_core::engine::Limits;
use graphrt_files::{Workspace, WorkspaceLimits};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const SECRET: &[u8] = b"test-jwt-secret-with-at-least-32-bytes";
const HEADER: &str = "X-User-Authorization";
const API_KEY: &str = "server-only-key";
/// Upload limit of the test servers, above their JSON body limit.
const MAX_UPLOAD: usize = 1024 * 1024;
const MAX_BODY: usize = 16 * 1024;
const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-serve-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn workspace(&self) -> PathBuf {
        self.0.join("ws")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A chat that answers with the files attached to the run.
fn write_workflow(dir: &TempDir) -> PathBuf {
    let flows = dir.0.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("chat.yaml"),
        r#"version: "1"
name: chat
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - id: end
      type: end
      outputs:
        - { variable: files, value_selector: [sys, files] }
  edges:
    - { source: start, target: end }
"#,
    )
    .unwrap();
    flows
}

#[derive(Clone, Copy, Default)]
struct Setup {
    api: Option<WorkspaceApi>,
    per_user: bool,
    read_only: bool,
    /// `--user-rate-limit`.
    per_minute: u32,
}

async fn try_app(dir: &TempDir, setup: Setup) -> Result<axum::Router, String> {
    let flows = write_workflow(dir);
    let files = discover(&flows).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        workspace: Some(WorkspaceOptions {
            dir: dir.workspace(),
            read_only: setup.read_only,
            per_user: setup.per_user,
            ..WorkspaceOptions::default()
        }),
        ..ServiceOptions::default()
    };
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let user_auth =
        UserAuth::with_keys(&specs[0], vec![VerifyKey::hs256(SECRET)], ClaimRules::default(), setup.per_minute);
    let state = build_state(&files, &options, Limits::default(), Some(API_KEY.into()))
        .await
        .expect("state")
        .with_header_inputs(&specs)
        .expect("header inputs")
        .with_user_auth(Some(user_auth))
        .with_workspace_api(setup.api, MAX_UPLOAD)
        .map_err(|e| e.to_string())?;
    Ok(router_with_uploads(Arc::new(state), MAX_BODY, MAX_UPLOAD))
}

async fn app(dir: &TempDir, setup: Setup) -> axum::Router {
    try_app(dir, setup).await.expect("app")
}

fn key_api() -> Setup {
    Setup { api: Some(WorkspaceApi::Key), ..Setup::default() }
}

fn users_api() -> Setup {
    Setup { api: Some(WorkspaceApi::Users), per_user: true, ..Setup::default() }
}

fn token_for(subject: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
    let claims = json!({ "sub": subject, "exp": now + 3600 });
    let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET), input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
}

/// Who a test request comes from.
#[derive(Clone, Copy)]
enum As<'a> {
    Key,
    User(&'a str),
    Nobody,
}

fn request(method: Method, uri: &str, who: As<'_>, body: impl Into<Body>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match who {
        As::Key => builder.header("authorization", format!("Bearer {API_KEY}")),
        As::User(subject) => builder.header(HEADER, format!("Bearer {}", token_for(subject))),
        As::Nobody => builder,
    };
    builder.body(body.into()).unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.expect("response");
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response.into_body().collect().await.map(|body| body.to_bytes().to_vec()).unwrap_or_default();
    (status, headers, body)
}

async fn call(app: &axum::Router, method: Method, uri: &str, who: As<'_>, body: impl Into<Body>) -> (StatusCode, Json) {
    let (status, _, bytes) = send(app, request(method, uri, who, body)).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
}

async fn upload(app: &axum::Router, who: As<'_>, query: &str, bytes: &[u8]) -> (StatusCode, Json) {
    call(app, Method::PUT, &format!("/v1/files/content?{query}"), who, bytes.to_vec()).await
}

async fn download(app: &axum::Router, who: As<'_>, query: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    send(app, request(Method::GET, &format!("/v1/files/content?{query}"), who, Body::empty())).await
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default()
}

/// Every file below `dir`, relative to it.
fn files_on_disk(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                found.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found.sort();
    found
}

#[tokio::test]
async fn the_routes_do_not_exist_unless_the_api_is_turned_on() {
    let dir = TempDir::new("off");
    let app = app(&dir, Setup::default()).await;
    for (method, uri) in [
        (Method::GET, "/v1/files"),
        (Method::GET, "/v1/files/info?path=a.txt"),
        (Method::GET, "/v1/files/content?path=a.txt"),
        (Method::PUT, "/v1/files/content?path=a.txt"),
        (Method::DELETE, "/v1/files/content?path=a.txt"),
    ] {
        let (status, _) = call(&app, method, uri, As::Key, "x").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(files_on_disk(&dir.workspace()).is_empty());
    // Attaching files to a run needs the API too.
    let body = json!({ "files": ["workspace:a.txt"] }).to_string();
    let start = request(Method::POST, "/v1/workflows/chat/runs", As::Key, body);
    let (parts, body) = start.into_parts();
    let mut start = Request::from_parts(parts, body);
    start.headers_mut().insert("content-type", "application/json".parse().unwrap());
    let (status, _, text) = send(&app, start).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&text).contains("--workspace-api"));
}

#[tokio::test]
async fn every_route_needs_the_api_key() {
    let dir = TempDir::new("auth");
    let app = app(&dir, key_api()).await;
    upload(&app, As::Key, "path=a.txt", b"hello").await;
    for (method, uri) in [
        (Method::GET, "/v1/files"),
        (Method::GET, "/v1/files/info?path=a.txt"),
        (Method::GET, "/v1/files/content?path=a.txt"),
        (Method::PUT, "/v1/files/content?path=b.txt"),
        (Method::DELETE, "/v1/files/content?path=a.txt"),
    ] {
        let (status, _, body) = send(&app, request(method.clone(), uri, As::Nobody, "x")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(!String::from_utf8_lossy(&body).contains("hello"));
        // A user token opens nothing while the API is for the key only.
        let (status, _, _) = send(&app, request(method, uri, As::User("user-1"), "x")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert_eq!(files_on_disk(&dir.workspace()), ["a.txt"]);
}

#[tokio::test]
async fn upload_list_info_download_and_delete() {
    let dir = TempDir::new("roundtrip");
    let app = app(&dir, key_api()).await;
    let csv = b"name,qty\npear,7\n";
    let (status, stored) = upload(&app, As::Key, "path=reports/stock.csv", csv).await;
    assert_eq!(status, StatusCode::CREATED, "{stored}");
    assert_eq!(stored["path"], "reports/stock.csv");
    assert_eq!(stored["size"], csv.len());
    assert_eq!(stored["mime_type"], "text/csv");
    assert_eq!(stored["inline_safe"], true);
    assert_eq!(stored["file"]["reference"], "workspace:reports/stock.csv");
    assert_eq!(std::fs::read(dir.workspace().join("reports/stock.csv")).unwrap(), csv);

    // A second upload of the same name needs `overwrite`.
    let (status, _) = upload(&app, As::Key, "path=reports/stock.csv", b"x").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = upload(&app, As::Key, "path=reports/stock.csv&overwrite=true", csv).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, listing) = call(&app, Method::GET, "/v1/files?dir=reports", As::Key, Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["files"][0]["path"], "reports/stock.csv");
    assert_eq!(listing["usage"], json!({ "files": 1, "bytes": csv.len() }));
    assert_eq!(listing["limits"]["max_file_bytes"], MAX_UPLOAD);

    let (status, info) = call(&app, Method::GET, "/v1/files/info?path=reports/stock.csv", As::Key, Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["sha256"], sha256_hex(csv).as_str());

    let (status, headers, body) = download(&app, As::Key, "path=reports/stock.csv").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, csv);
    assert_eq!(header(&headers, "content-type"), "text/csv; charset=utf-8");
    assert_eq!(header(&headers, "content-length"), csv.len().to_string());
    assert_eq!(
        header(&headers, "content-disposition"),
        "attachment; filename=\"stock.csv\"; filename*=UTF-8''stock.csv"
    );
    assert_eq!(header(&headers, "x-content-type-options"), "nosniff");
    assert_eq!(header(&headers, "cache-control"), "private, no-store");
    // The same file by the reference its file value carries.
    let (status, _, body) = download(&app, As::Key, "ref=workspace:reports/stock.csv").await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &csv[..]));

    let (status, _) =
        call(&app, Method::DELETE, "/v1/files/content?path=reports/stock.csv", As::Key, Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = download(&app, As::Key, "path=reports/stock.csv").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tokio::test]
async fn a_large_file_downloads_whole_and_in_order() {
    let dir = TempDir::new("large");
    let app = app(&dir, key_api()).await;
    // Larger than the JSON body limit of the server, and three steps of a download.
    let bytes: Vec<u8> = (0..700_000u32).map(|i| b'a' + (i % 23) as u8).collect();
    let (status, stored) = upload(&app, As::Key, "path=big.txt", &bytes).await;
    assert_eq!(status, StatusCode::CREATED, "{stored}");
    let (status, headers, body) = download(&app, As::Key, "path=big.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-length"), "700000");
    assert!(body == bytes);
}

#[tokio::test]
async fn a_path_from_a_client_never_leaves_the_workspace() {
    let dir = TempDir::new("traversal");
    let app = app(&dir, key_api()).await;
    upload(&app, As::Key, "path=inside.txt", b"inside").await;
    std::fs::write(dir.0.join("outside.txt"), b"secret").unwrap();
    let escapes = [
        "../outside.txt",
        "..%2Foutside.txt",
        "%2e%2e/outside.txt",
        "a/../../outside.txt",
        "/etc/passwd",
        "%2Fetc%2Fpasswd",
        "..%5Coutside.txt",
        "C:%5CWindows%5Cwin.ini",
        "a%00.txt",
        ".env",
        "scopes/0123456789abcdef0123456789abcdef/x.txt",
        "",
    ];
    for path in escapes {
        for (method, route) in [
            (Method::GET, "/v1/files/content"),
            (Method::GET, "/v1/files/info"),
            (Method::PUT, "/v1/files/content"),
            (Method::DELETE, "/v1/files/content"),
        ] {
            let uri = format!("{route}?path={path}");
            let (status, _, body) = send(&app, request(method.clone(), &uri, As::Key, "evil")).await;
            assert!(status.is_client_error(), "{method} {uri} answered {status}");
            assert!(!String::from_utf8_lossy(&body).contains("secret"), "{uri}");
        }
        let (status, _) = call(&app, Method::GET, &format!("/v1/files?dir={path}x/.."), As::Key, Body::empty()).await;
        assert!(status.is_client_error(), "list {path}");
    }
    // Neither does a reference, which a client may also send.
    for reference in ["workspace:../outside.txt", "workspace:/etc/passwd", "local:outside.txt", "../outside.txt"] {
        let (status, _, body) = download(&app, As::Key, &format!("ref={reference}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{reference}");
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
    }
    assert_eq!(files_on_disk(&dir.workspace()), ["inside.txt"]);
    assert_eq!(std::fs::read(dir.0.join("outside.txt")).unwrap(), b"secret");
}

#[cfg(unix)]
#[tokio::test]
async fn a_link_planted_in_the_workspace_is_not_served() {
    let dir = TempDir::new("link");
    let app = app(&dir, key_api()).await;
    upload(&app, As::Key, "path=real.txt", b"real").await;
    std::fs::write(dir.0.join("outside.txt"), b"secret").unwrap();
    std::os::unix::fs::symlink(dir.0.join("outside.txt"), dir.workspace().join("link.txt")).unwrap();
    for uri in ["/v1/files/content?path=link.txt", "/v1/files/info?path=link.txt"] {
        let (status, _, body) = send(&app, request(Method::GET, uri, As::Key, Body::empty())).await;
        assert!(status.is_client_error(), "{uri} answered {status}");
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
    }
    let (status, _) = upload(&app, As::Key, "path=link.txt&overwrite=true", b"overwritten").await;
    assert!(status.is_client_error());
    assert_eq!(std::fs::read(dir.0.join("outside.txt")).unwrap(), b"secret");
}

#[tokio::test]
async fn active_content_is_a_download_and_never_a_page() {
    let dir = TempDir::new("active");
    let app = app(&dir, key_api()).await;
    let page = b"<!doctype html><html><script>fetch('/v1/runs')</script></html>";
    let drawing = b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"></svg>";
    // The names lie about the type on purpose.
    for (name, bytes, mime) in [
        ("page.html", &page[..], "text/html"),
        ("photo.png", &drawing[..], "image/svg+xml"),
        ("notes.txt", page, "text/html"),
    ] {
        let (status, stored) = upload(&app, As::Key, &format!("path={name}"), bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{stored}");
        assert_eq!(stored["mime_type"], mime, "{name}");
        assert_eq!(stored["inline_safe"], false, "{name}");
        for query in [format!("path={name}"), format!("path={name}&inline=true")] {
            let (status, headers, body) = download(&app, As::Key, &query).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, bytes);
            assert_eq!(header(&headers, "content-type"), "application/octet-stream", "{query}");
            assert!(header(&headers, "content-disposition").starts_with("attachment;"), "{query}");
            assert_eq!(header(&headers, "x-content-type-options"), "nosniff", "{query}");
            assert_eq!(header(&headers, "content-security-policy"), "sandbox; default-src 'none'", "{query}");
            assert_eq!(header(&headers, "cross-origin-resource-policy"), "same-origin", "{query}");
        }
    }
    // An image is shown inline only when the client asks, and text only as plain text.
    let png = graphrt_core::file::base64_decode(PNG_1X1).unwrap();
    upload(&app, As::Key, "path=dot.png", &png).await;
    let (_, headers, _) = download(&app, As::Key, "path=dot.png&inline=true").await;
    assert_eq!(header(&headers, "content-type"), "image/png");
    assert!(header(&headers, "content-disposition").starts_with("inline;"));
    assert_eq!(header(&headers, "x-content-type-options"), "nosniff");
    let (_, headers, _) = download(&app, As::Key, "path=dot.png").await;
    assert!(header(&headers, "content-disposition").starts_with("attachment;"));
    upload(&app, As::Key, "path=data.xml", b"<?xml version=\"1.0\"?><a><b>1</b></a>").await;
    let (_, headers, _) = download(&app, As::Key, "path=data.xml&inline=true").await;
    assert_eq!(header(&headers, "content-type"), "text/plain; charset=utf-8");
}

#[tokio::test]
async fn programs_cannot_be_uploaded_whatever_they_are_called() {
    let dir = TempDir::new("programs");
    let app = app(&dir, key_api()).await;
    let programs: [(&str, &[u8]); 3] = [
        ("photo.png", b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0"),
        ("notes.txt", b"MZ\x90\0\x03\0\0\0\x04\0\0\0"),
        ("run.md", b"#!/bin/sh\nrm -rf /\n"),
    ];
    for (name, bytes) in programs {
        let (status, body) = upload(&app, As::Key, &format!("path={name}"), bytes).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
    }
    assert!(files_on_disk(&dir.workspace()).is_empty());
}

#[tokio::test]
async fn an_upload_over_the_limit_is_refused_and_nothing_is_stored() {
    let dir = TempDir::new("limit");
    let app = app(&dir, key_api()).await;
    // Announced: refused from the header.
    let (status, body) = upload(&app, As::Key, "path=big.bin", &vec![b'a'; MAX_UPLOAD + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    // Not announced (chunked): refused while it arrives.
    let pieces = (0..=MAX_UPLOAD / 1024).map(|_| Ok::<_, std::io::Error>(vec![b'a'; 1024]));
    let chunked = Body::from_stream(futures::stream::iter(pieces));
    let (status, _) = call(&app, Method::PUT, "/v1/files/content?path=big.bin", As::Key, chunked).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // At the limit, which is above the JSON body limit of the server: stored.
    let (status, _) = upload(&app, As::Key, "path=fits.txt", &vec![b'a'; MAX_UPLOAD]).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = upload(&app, As::Key, "path=empty.txt", b"").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(files_on_disk(&dir.workspace()), ["fits.txt"]);
}

#[tokio::test]
async fn a_read_only_workspace_serves_files_and_refuses_changes() {
    let dir = TempDir::new("readonly");
    std::fs::create_dir_all(dir.workspace()).unwrap();
    std::fs::write(dir.workspace().join("a.txt"), b"kept").unwrap();
    let app = app(&dir, Setup { read_only: true, ..key_api() }).await;
    let (status, _, body) = download(&app, As::Key, "path=a.txt").await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"kept"[..]));
    let (status, _) = upload(&app, As::Key, "path=b.txt", b"new").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = upload(&app, As::Key, "path=a.txt&overwrite=true", b"replaced").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, Method::DELETE, "/v1/files/content?path=a.txt", As::Key, Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(std::fs::read(dir.workspace().join("a.txt")).unwrap(), b"kept");
}

#[tokio::test]
async fn users_only_reach_their_own_directory() {
    let dir = TempDir::new("users");
    let app = app(&dir, users_api()).await;
    let (ana, budi) = (As::User("ana"), As::User("budi"));
    let (status, stored) = upload(&app, ana, "path=uploads/brief.txt", b"for ana only").await;
    assert_eq!(status, StatusCode::CREATED, "{stored}");
    let reference = stored["file"]["reference"].as_str().unwrap().to_owned();
    assert!(reference.starts_with("workspace:scopes/"), "{reference}");

    // The file is where the `files` tools of an agent look for this user's scope.
    let workspace = Workspace::dir(dir.workspace(), WorkspaceLimits::default()).unwrap();
    let seen_by_agent = workspace.scoped("ana").read_all(&"uploads/brief.txt".parse().unwrap(), 1024).await;
    assert_eq!(seen_by_agent.unwrap(), b"for ana only");

    // Another user sees nothing of it: not by path, not by reference, not in a listing.
    let (status, _, body) = download(&app, budi, "path=uploads/brief.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!String::from_utf8_lossy(&body).contains("ana"));
    for uri in [format!("/v1/files/content?ref={reference}"), format!("/v1/files/info?ref={reference}")] {
        let (status, _, body) = send(&app, request(Method::GET, &uri, budi, Body::empty())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!String::from_utf8_lossy(&body).contains("for ana"));
    }
    let (status, _) =
        call(&app, Method::DELETE, &format!("/v1/files/content?ref={reference}"), budi, Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listing) = call(&app, Method::GET, "/v1/files", budi, Body::empty()).await;
    assert_eq!(listing["files"], json!([]));
    assert_eq!(listing["usage"]["files"], 0);
    // Nor can a user name a scope, their own included, or walk into the scopes directory.
    let scope_dir = reference.trim_start_matches("workspace:").trim_end_matches("/uploads/brief.txt");
    for uri in [
        "/v1/files?scope=ana".to_owned(),
        "/v1/files/content?path=uploads/brief.txt&scope=ana".to_owned(),
        "/v1/files/content?path=uploads/brief.txt&scope=budi".to_owned(),
        format!("/v1/files/content?path={scope_dir}/uploads/brief.txt"),
        format!("/v1/files/content?path=../{}/uploads/brief.txt", scope_dir.trim_start_matches("scopes/")),
    ] {
        let (status, _, body) = send(&app, request(Method::GET, &uri, budi, Body::empty())).await;
        assert!(status.is_client_error(), "{uri} answered {status}");
        assert!(!String::from_utf8_lossy(&body).contains("for ana"), "{uri}");
    }
    // The same name is a different file for the other user.
    let (status, _) = upload(&app, budi, "path=uploads/brief.txt", b"for budi").await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, body) = download(&app, ana, &format!("ref={reference}")).await;
    assert_eq!(body, b"for ana only");

    // The API key reaches any user by naming the scope, and must name one.
    let (status, _, body) = download(&app, As::Key, "path=uploads/brief.txt&scope=ana").await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"for ana only"[..]));
    let (status, _, body) = download(&app, As::Key, &format!("ref={reference}")).await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"for ana only"[..]));
    let (status, _) = call(&app, Method::GET, "/v1/files", As::Key, Body::empty()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = upload(&app, As::Key, "path=shared.txt", b"x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_invalid_user_token_opens_nothing_even_next_to_the_key() {
    let dir = TempDir::new("badtoken");
    let app = app(&dir, users_api()).await;
    let forged = Request::builder()
        .method(Method::PUT)
        .uri("/v1/files/content?path=a.txt")
        .header(HEADER, "Bearer not-a-token")
        .header("authorization", format!("Bearer {API_KEY}"))
        .body(Body::from("x"))
        .unwrap();
    let (status, _, _) = send(&app, forged).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(files_on_disk(&dir.workspace()).is_empty());
}

#[tokio::test]
async fn opening_the_api_to_users_needs_a_directory_per_user() {
    let dir = TempDir::new("config");
    let shared = Setup { api: Some(WorkspaceApi::Users), per_user: false, ..Setup::default() };
    let error = try_app(&dir, shared).await.expect_err("refused");
    assert!(error.contains("--workspace-per-user"), "{error}");
    // In a shared workspace the key cannot name a scope either.
    let app = app(&dir, key_api()).await;
    let (status, _) = upload(&app, As::Key, "path=a.txt&scope=ana", b"x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn file_requests_do_not_spend_the_run_rate_limit() {
    let dir = TempDir::new("ratelimit");
    let app = app(&dir, Setup { per_minute: 1, ..users_api() }).await;
    for _ in 0..5 {
        let (status, _) = call(&app, Method::GET, "/v1/files", As::User("ana"), Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
    }
}

fn start_run(who: As<'_>, body: &Json) -> Request<Body> {
    let mut start = request(Method::POST, "/v1/workflows/chat/runs", who, body.to_string());
    start.headers_mut().insert("content-type", "application/json".parse().unwrap());
    start
}

#[tokio::test]
async fn an_attached_upload_reaches_the_run_as_a_file_value() {
    let dir = TempDir::new("attach");
    let app = app(&dir, users_api()).await;
    let png = graphrt_core::file::base64_decode(PNG_1X1).unwrap();
    // The declared name says text. The file value says what the bytes are.
    let (_, stored) = upload(&app, As::User("ana"), "path=uploads/photo.txt", &png).await;
    let reference = stored["file"]["reference"].as_str().unwrap().to_owned();
    let (status, _, body) = send(&app, start_run(As::User("ana"), &json!({ "files": [reference] }))).await;
    let result: Json = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    let file = &result["outputs"]["files"][0];
    assert_eq!(file["$file"], true, "{result}");
    assert_eq!(file["reference"], reference.as_str());
    assert_eq!(file["mime_type"], "image/png");
    assert_eq!(file["kind"], "image");
}

#[tokio::test]
async fn a_run_cannot_attach_another_users_file_or_forge_one() {
    let dir = TempDir::new("attach-other");
    let app = app(&dir, users_api()).await;
    let (_, stored) = upload(&app, As::User("ana"), "path=uploads/secret.txt", b"for ana only").await;
    let reference = stored["file"]["reference"].as_str().unwrap().to_owned();
    for files in [
        json!([reference]),
        json!(["workspace:../../etc/passwd"]),
        json!(["workspace:uploads/secret.txt"]),
        json!(["/etc/passwd"]),
    ] {
        let (status, _, body) = send(&app, start_run(As::User("budi"), &json!({ "files": files }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{files}");
        assert!(String::from_utf8_lossy(&body).contains("is not a file of this workspace"), "{files}");
    }
    let too_many: Vec<&str> = std::iter::repeat_n(reference.as_str(), 17).collect();
    let (status, _, _) = send(&app, start_run(As::User("ana"), &json!({ "files": too_many }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A file value written by hand into `sys.files` stays plain data: it is not a file.
    let forged = json!({ "sys": { "files": [{ "$file": true, "name": "x", "transfer": "tool_file", "reference": reference }] } });
    let (status, _, body) = send(&app, start_run(As::User("budi"), &forged)).await;
    let result: Json = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(result["outputs"]["files"][0].get("kind").is_none(), "{result}");
}

/// Without an API key, anyone who reaches the port could read and replace the
/// agents' files, so the server does not start on an address other hosts reach.
#[test]
fn the_api_is_not_served_to_a_network_without_an_api_key() {
    let dir = TempDir::new("keyless");
    let flows = write_workflow(&dir);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", flows.to_str().unwrap(), "--addr", "0.0.0.0:0", "--no-default-plugins"])
        .args(["--workspace", dir.workspace().to_str().unwrap(), "--workspace-api", "key"])
        .env_remove("GRAPHRT_API_KEY")
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert!(report.contains("`--workspace-api` on an address other hosts can reach needs an API key"), "{report}");
    // The flag means nothing without a workspace.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["serve", flows.to_str().unwrap(), "--workspace-api", "key"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--workspace"));
}
