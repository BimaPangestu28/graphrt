//! `S3Store` against a local server that speaks the S3 requests the store
//! sends and checks every signature and body hash it receives.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use graphrt_files::{FileStore, FilesError, S3Config, S3Store, StoreError, Workspace, WorkspaceLimits, WorkspacePath};
use graphrt_provider_aws::auth::sigv4::{self, AwsCredentials, SigningRequest};

const ACCESS_KEY: &str = "AKIAFAKEFAKEFAKEFAKE";
const SECRET_KEY: &str = "fake-secret-key-that-must-never-be-printed";
const SESSION_TOKEN: &str = "fake-session-token-that-must-never-be-printed";
const BUCKET: &str = "agents";
const REGION: &str = "eu-test-1";

/// A one bucket S3 server held in memory.
#[derive(Default)]
struct FakeS3 {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// `METHOD path?query` of every request, as received.
    log: Mutex<Vec<String>>,
    /// Requests refused because their signature or body hash was wrong.
    unsigned: AtomicUsize,
    /// Answer this many requests with `503 SlowDown` first.
    fail_first: AtomicU32,
    /// Keys per listing page (`0`: what the client asked for).
    page_size: AtomicUsize,
    ignore_range: AtomicBool,
    ignore_conditions: AtomicBool,
    flood_listing: AtomicBool,
    endless_listing: AtomicBool,
    stall: AtomicBool,
    redirect_to: Mutex<Option<String>>,
}

impl FakeS3 {
    fn put(&self, key: &str, bytes: &[u8]) {
        self.objects.lock().unwrap().insert(key.to_owned(), bytes.to_vec());
    }

    fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|digit| (*digit as char).to_digit(16));
        match (bytes[index], hex(index + 1).zip(hex(index + 2))) {
            (b'%', Some((high, low))) => {
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn answer(status: StatusCode, body: impl Into<Body>) -> Response {
    Response::builder().status(status).body(body.into()).unwrap()
}

fn error(status: StatusCode, code: &str) -> Response {
    answer(
        status,
        format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{code} happened</Message></Error>"),
    )
}

/// Whether the request carries the signature the real signer produces for it.
fn signature_is_valid(
    method: &Method,
    path: &str,
    query: &[(String, String)],
    headers: &HeaderMap,
    body: &[u8],
) -> bool {
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let authorization = value("authorization");
    let Some(signed) = authorization.split("SignedHeaders=").nth(1).and_then(|rest| rest.split(',').next()) else {
        return false;
    };
    let signed: Vec<(String, String)> = signed.split(';').map(|name| (name.to_owned(), value(name))).collect();
    let must_sign = ["host", "x-amz-date", "x-amz-content-sha256", "x-amz-security-token"];
    if !must_sign.iter().all(|name| signed.iter().any(|(signed, _)| signed == name)) {
        return false;
    }
    for conditional in ["range", "if-none-match"] {
        if headers.contains_key(conditional) && !signed.iter().any(|(name, _)| name == conditional) {
            return false;
        }
    }
    let payload_hash = value("x-amz-content-sha256");
    if payload_hash != sigv4::sha256_hex(body) {
        return false;
    }
    let credentials = AwsCredentials {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET_KEY.into(),
        session_token: Some(SESSION_TOKEN.into()),
    };
    let query: Vec<(&str, &str)> = query.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let signed: Vec<(&str, &str)> = signed.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let amz_date = value("x-amz-date");
    let expected = sigv4::authorization(
        &credentials,
        &SigningRequest {
            method: method.as_str(),
            path,
            query: &query,
            headers: &signed,
            payload_hash: &payload_hash,
            region: REGION,
            service: "s3",
            amz_date: &amz_date,
            double_encode_path: false,
        },
    );
    expected == authorization && value("x-amz-security-token") == SESSION_TOKEN
}

async fn handle(State(fake): State<Arc<FakeS3>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let path = parts.uri.path().to_owned();
    let raw_query = parts.uri.query().unwrap_or_default().to_owned();
    let shown = if raw_query.is_empty() { path.clone() } else { format!("{path}?{raw_query}") };
    fake.log.lock().unwrap().push(format!("{} {shown}", parts.method));
    if fake.stall.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let query: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect();
    if !signature_is_valid(&parts.method, &path, &query, &parts.headers, &body) {
        fake.unsigned.fetch_add(1, Ordering::Relaxed);
        return error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch");
    }
    if fake.fail_first.load(Ordering::Relaxed) > 0 {
        fake.fail_first.fetch_sub(1, Ordering::Relaxed);
        return error(StatusCode::SERVICE_UNAVAILABLE, "SlowDown");
    }
    if let Some(location) = fake.redirect_to.lock().unwrap().clone() {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", format!("{location}{shown}"))
            .body(Body::from("<Error><Code>TemporaryRedirect</Code></Error>"))
            .unwrap();
    }
    let bucket_root = format!("/{BUCKET}");
    if path == bucket_root {
        return list(&fake, &query);
    }
    let Some(key) = path.strip_prefix(&format!("{bucket_root}/")).map(percent_decode) else {
        return error(StatusCode::NOT_FOUND, "NoSuchBucket");
    };
    let mut objects = fake.objects.lock().unwrap();
    match parts.method {
        Method::PUT => {
            let conditional = parts.headers.get("if-none-match").is_some();
            if conditional && !fake.ignore_conditions.load(Ordering::Relaxed) && objects.contains_key(&key) {
                return error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
            }
            objects.insert(key, body.to_vec());
            answer(StatusCode::OK, Body::empty())
        }
        Method::DELETE => {
            objects.remove(&key);
            answer(StatusCode::NO_CONTENT, Body::empty())
        }
        Method::GET | Method::HEAD => {
            let Some(bytes) = objects.get(&key) else { return error(StatusCode::NOT_FOUND, "NoSuchKey") };
            let range = parts.headers.get("range").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes="));
            let last_modified = ("last-modified", "Fri, 27 Nov 2026 01:02:03 GMT");
            match range.filter(|_| !fake.ignore_range.load(Ordering::Relaxed)) {
                Some(range) => {
                    let (start, end) = range.split_once('-').unwrap();
                    let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
                    if start >= bytes.len() {
                        return error(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange");
                    }
                    let slice = bytes[start..bytes.len().min(end + 1)].to_vec();
                    Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(last_modified.0, last_modified.1)
                        .body(Body::from(slice))
                        .unwrap()
                }
                None => Response::builder()
                    .status(StatusCode::OK)
                    .header(last_modified.0, last_modified.1)
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            }
        }
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed"),
    }
}

fn list(fake: &FakeS3, query: &[(String, String)]) -> Response {
    let param = |name: &str| query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    if param("list-type") != Some("2") {
        return error(StatusCode::BAD_REQUEST, "InvalidArgument");
    }
    if fake.flood_listing.load(Ordering::Relaxed) {
        return answer(StatusCode::OK, vec![b' '; 5 * 1024 * 1024]);
    }
    let prefix = param("prefix").unwrap_or_default();
    let asked: usize = param("max-keys").and_then(|n| n.parse().ok()).unwrap_or(1000);
    let page_size = match fake.page_size.load(Ordering::Relaxed) {
        0 => asked,
        forced => forced,
    };
    let after = param("continuation-token").unwrap_or_default();
    let objects = fake.objects.lock().unwrap();
    let mut matching = objects.iter().filter(|(key, _)| key.starts_with(prefix) && key.as_str() > after);
    let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    let mut last = after.to_owned();
    for (key, bytes) in matching.by_ref().take(page_size) {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2026-11-27T01:02:03.000Z</LastModified><Size>{}</Size></Contents>",
            xml_escape(key),
            bytes.len()
        ));
        last.clone_from(key);
    }
    let more = matching.next().is_some() || fake.endless_listing.load(Ordering::Relaxed);
    xml.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
    if more {
        xml.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", xml_escape(&last)));
    }
    xml.push_str("</ListBucketResult>");
    answer(StatusCode::OK, xml)
}

/// Start a fake S3 and return it with its `http://127.0.0.1:port`.
async fn serve() -> (Arc<FakeS3>, String) {
    let fake = Arc::new(FakeS3::default());
    let app = axum::Router::new().fallback(handle).with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, endpoint)
}

fn credentials(secret: &'static str) -> impl Fn(&str) -> Option<String> {
    move |name| match name {
        "AWS_ACCESS_KEY_ID" => Some(ACCESS_KEY.to_owned()),
        "AWS_SECRET_ACCESS_KEY" => Some(secret.to_owned()),
        "AWS_SESSION_TOKEN" => Some(SESSION_TOKEN.to_owned()),
        "AWS_EC2_METADATA_DISABLED" => Some("true".to_owned()),
        _ => None,
    }
}

fn config(endpoint: &str, prefix: Option<&str>) -> S3Config {
    S3Config {
        endpoint: Some(endpoint.to_owned()),
        prefix: prefix.map(str::to_owned),
        allow_http: true,
        request_timeout: Duration::from_secs(5),
        ..S3Config::new(BUCKET, REGION)
    }
}

fn store(endpoint: &str, prefix: Option<&str>) -> S3Store {
    S3Store::from_lookup(&config(endpoint, prefix), credentials(SECRET_KEY)).unwrap()
}

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

#[tokio::test]
async fn files_are_objects_below_the_prefix() {
    let (fake, endpoint) = serve().await;
    let store = store(&endpoint, Some("tenants/acme"));
    assert_eq!(store.stat(&path("notes/a.md")).await.unwrap(), None);
    let written = store.write(&path("notes/a.md"), b"# hello\n".to_vec(), false).await.unwrap();
    assert_eq!((written.path.as_str(), written.size), ("notes/a.md", 8));
    assert_eq!(fake.keys(), ["tenants/acme/notes/a.md"]);

    let entry = store.stat(&path("notes/a.md")).await.unwrap().unwrap();
    assert_eq!((entry.size, entry.modified_ms), (8, Some(1_795_741_323_000)));
    assert_eq!(store.read(&path("notes/a.md"), 0, 100).await.unwrap(), b"# hello\n");
    assert_eq!(store.read(&path("notes/a.md"), 2, 3).await.unwrap(), b"hel");
    // Reading at or past the end returns nothing, like every other store.
    assert_eq!(store.read(&path("notes/a.md"), 8, 10).await.unwrap(), b"");
    assert_eq!(store.read(&path("notes/a.md"), 500, 10).await.unwrap(), b"");
    assert_eq!(store.read(&path("notes/a.md"), 0, 0).await.unwrap(), b"");
    assert!(matches!(store.read(&path("missing.md"), 0, 10).await, Err(StoreError::NotFound(p)) if p == "missing.md"));
    assert!(matches!(store.read(&path("missing.md"), 0, 0).await, Err(StoreError::NotFound(_))));
    let digest = graphrt_files::hex(&store.sha256(&path("notes/a.md")).await.unwrap());
    assert_eq!(digest, sigv4::sha256_hex(b"# hello\n"));

    store.write(&path("notes/b.md"), b"b".to_vec(), false).await.unwrap();
    store.write(&path("top.txt"), b"top".to_vec(), false).await.unwrap();
    let all = store.list(None, 10).await.unwrap();
    let names: Vec<&str> = all.entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["notes/a.md", "notes/b.md", "top.txt"]);
    assert!(!all.truncated);
    let notes = store.list(Some(&path("notes")), 10).await.unwrap();
    assert_eq!(notes.entries.len(), 2);
    assert_eq!(notes.entries[0].modified_ms, Some(1_795_741_323_000));
    assert!(store.list(Some(&path("nothing")), 10).await.unwrap().entries.is_empty());
    let cut = store.list(None, 2).await.unwrap();
    assert_eq!((cut.entries.len(), cut.truncated), (2, true));

    store.delete(&path("notes/a.md")).await.unwrap();
    assert_eq!(store.stat(&path("notes/a.md")).await.unwrap(), None);
    assert_eq!(fake.unsigned.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_workspace_keeps_its_rules_over_object_storage() {
    let (fake, endpoint) = serve().await;
    let limits =
        WorkspaceLimits { max_file_bytes: 64, max_total_bytes: 100, max_files: 3, ..WorkspaceLimits::default() };
    let workspace = Workspace::new(Arc::new(store(&endpoint, Some("ws"))), limits);
    workspace.write(&path("a.txt"), vec![b'a'; 60], false).await.unwrap();
    // Size, quota and content rules live above the store, so they hold here too.
    assert!(matches!(workspace.write(&path("b.txt"), vec![b'b'; 65], false).await, Err(FilesError::TooLarge { .. })));
    assert!(matches!(
        workspace.write(&path("b.txt"), vec![b'b'; 41], false).await,
        Err(FilesError::QuotaExceeded { .. })
    ));
    assert!(matches!(
        workspace.write(&path("run.txt"), b"\x7fELF\x02\x01\x01\0".to_vec(), false).await,
        Err(FilesError::Refused { .. })
    ));
    assert!(matches!(
        workspace.write(&path("a.txt"), b"again".to_vec(), false).await,
        Err(FilesError::Store(StoreError::AlreadyExists(_)))
    ));
    // Users are separate key prefixes, and a second process counts what is there.
    let ana = workspace.scoped("ana");
    ana.write(&path("mine.txt"), b"ana".to_vec(), false).await.unwrap();
    assert!(workspace.scoped("budi").list(None, 10).await.unwrap().entries.is_empty());
    assert!(workspace.scoped("budi").read(&path("mine.txt"), 0, 10).await.is_err());
    assert_eq!(workspace.list(None, 10).await.unwrap().entries.len(), 1);
    let restarted = Workspace::new(Arc::new(store(&endpoint, Some("ws"))), WorkspaceLimits::default());
    assert_eq!(restarted.usage().await.unwrap().bytes, 60);
    assert_eq!(restarted.scoped("ana").usage().await.unwrap().files, 1);
    assert!(fake.keys().iter().all(|key| key.starts_with("ws/")), "{:?}", fake.keys());
    assert_eq!(fake.unsigned.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn every_request_is_signed_and_a_wrong_key_is_refused() {
    let (fake, endpoint) = serve().await;
    let good = store(&endpoint, None);
    good.write(&path("a.txt"), b"signed".to_vec(), false).await.unwrap();
    good.read(&path("a.txt"), 1, 2).await.unwrap();
    good.list(None, 10).await.unwrap();
    good.stat(&path("a.txt")).await.unwrap();
    // The fake recomputes each signature with the real signer, body hash included.
    assert_eq!(fake.unsigned.load(Ordering::Relaxed), 0);
    assert!(fake.log().len() >= 4);

    let wrong = S3Store::from_lookup(&config(&endpoint, None), credentials("another-secret")).unwrap();
    for refused in [
        wrong.read(&path("a.txt"), 0, 10).await.map(|_| ()),
        wrong.write(&path("b.txt"), b"x".to_vec(), true).await.map(|_| ()),
        wrong.delete(&path("a.txt")).await,
        wrong.list(None, 10).await.map(|_| ()),
    ] {
        assert!(matches!(refused, Err(StoreError::Denied { .. })), "{refused:?}");
    }
    assert_eq!(fake.keys(), ["a.txt"]);
}

#[tokio::test]
async fn a_path_cannot_reach_another_prefix_the_bucket_or_the_query() {
    let (fake, endpoint) = serve().await;
    fake.put("tenants/other/secret.txt", b"other tenant");
    let store = store(&endpoint, Some("tenants/acme"));
    // Every character a workspace path allows that means something in a URL.
    let names = [
        "a&versionId=1&list-type=2.txt",
        "a#fragment.txt",
        "%2e%2e/%2e%2e/other/secret.txt",
        "x..%2fother%2fsecret.txt",
        "a b+c.txt",
        "é ü 漢字.txt",
        "a;b=c@d$e,f.txt",
        "~user/file.txt",
    ];
    for name in names {
        let written = store.write(&path(name), name.as_bytes().to_vec(), false).await;
        assert!(written.is_ok(), "{name}: {written:?}");
        assert_eq!(store.read(&path(name), 0, 1000).await.unwrap(), name.as_bytes(), "{name}");
    }
    let expected: Vec<String> = names.iter().map(|name| format!("tenants/acme/{name}")).collect();
    for key in &expected {
        assert!(fake.keys().contains(key), "{key} missing from {:?}", fake.keys());
    }
    // Nothing was sent outside the prefix, and no name became a query.
    for line in fake.log() {
        let target = line.split(' ').nth(1).unwrap();
        assert!(target.starts_with("/agents/tenants/acme/"), "{line}");
        assert!(!target.contains('?') && !target.contains('#'), "{line}");
        assert!(!target.split('/').any(|segment| segment == ".."), "{line}");
    }
    assert_eq!(fake.objects.lock().unwrap()["tenants/other/secret.txt"], b"other tenant");
    assert_eq!(fake.unsigned.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn objects_that_are_not_workspace_files_are_not_listed() {
    let (fake, endpoint) = serve().await;
    for key in [
        "ws/kept.txt",
        "ws/dir/kept.txt",
        "ws/.env",
        "ws/../escaped.txt",
        "ws/a/../../escaped.txt",
        "ws//double.txt",
        "ws/back\\slash.txt",
        "ws/report\u{202e}txt.exe",
        "ws/con",
        "wsx/sibling.txt",
        "other/kept.txt",
        "kept.txt",
    ] {
        fake.put(key, b"x");
    }
    let store = store(&endpoint, Some("ws"));
    let listing = store.list(None, 100).await.unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["dir/kept.txt", "kept.txt"]);
    // The same through pages of two keys.
    fake.page_size.store(2, Ordering::Relaxed);
    let paged = store.list(None, 100).await.unwrap();
    assert_eq!(paged.entries, listing.entries);
    assert!(!paged.truncated);
}

#[tokio::test]
async fn a_create_does_not_replace_what_another_process_wrote() {
    let (fake, endpoint) = serve().await;
    let (first, second) = (store(&endpoint, None), store(&endpoint, None));
    first.write(&path("report.md"), b"first".to_vec(), false).await.unwrap();
    // The second process never saw the file: the store itself refuses.
    let lost = second.write(&path("report.md"), b"second".to_vec(), false).await;
    assert!(matches!(lost, Err(StoreError::AlreadyExists(p)) if p == "report.md"));
    assert_eq!(fake.objects.lock().unwrap()["report.md"], b"first");
    second.write(&path("report.md"), b"second".to_vec(), true).await.unwrap();
    assert_eq!(fake.objects.lock().unwrap()["report.md"], b"second");
    let conditional = fake.log().iter().filter(|line| line.starts_with("PUT")).count();
    assert_eq!(conditional, 3);
}

#[tokio::test]
async fn a_server_that_sends_too_much_is_cut_off() {
    let (fake, endpoint) = serve().await;
    fake.put("big.bin", &vec![7u8; 3 * 1024 * 1024]);
    let store = store(&endpoint, None);
    // A store that ignores the range gets no more than was asked for into memory.
    fake.ignore_range.store(true, Ordering::Relaxed);
    assert_eq!(store.read(&path("big.bin"), 0, 1024).await.unwrap().len(), 1024);
    let shifted = store.read(&path("big.bin"), 10, 1024).await;
    assert!(
        matches!(shifted, Err(StoreError::Io { message, .. }) if message.contains("ignored the requested byte range"))
    );
    // A listing has a size limit, and a store that always has "more" is not followed forever.
    fake.flood_listing.store(true, Ordering::Relaxed);
    let flooded = store.list(None, 10).await;
    assert!(matches!(flooded, Err(StoreError::Io { message, .. }) if message.contains("too large")));
    fake.flood_listing.store(false, Ordering::Relaxed);
    fake.endless_listing.store(true, Ordering::Relaxed);
    let before = fake.log().len();
    let endless = store.list(None, 10).await.unwrap();
    assert!(endless.truncated);
    assert_eq!(fake.log().len() - before, 64);
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (fake, endpoint) = serve().await;
    let (other, other_endpoint) = serve().await;
    other.put("a.txt", b"from another host");
    fake.put("a.txt", b"here");
    *fake.redirect_to.lock().unwrap() = Some(other_endpoint);
    let store = store(&endpoint, None);
    let read = store.read(&path("a.txt"), 0, 100).await;
    assert!(matches!(&read, Err(StoreError::Io { message, .. }) if message.contains("redirected")), "{read:?}");
    let written = store.write(&path("b.txt"), b"x".to_vec(), true).await;
    assert!(written.is_err());
    // The signed requests never reached the other host.
    assert!(other.log().is_empty());
    assert_eq!(other.keys(), ["a.txt"]);
}

#[tokio::test]
async fn secrets_never_appear_in_errors_or_debug_output() {
    let (fake, endpoint) = serve().await;
    let store = store(&endpoint, Some("ws"));
    let mut seen = format!("{store:?}");
    fake.fail_first.store(10, Ordering::Relaxed);
    seen.push_str(&format!("{:?}", store.read(&path("a.txt"), 0, 1).await));
    fake.fail_first.store(0, Ordering::Relaxed);
    *fake.redirect_to.lock().unwrap() = Some("http://127.0.0.1:1".into());
    seen.push_str(&format!("{:?}", store.stat(&path("a.txt")).await));
    let wrong = S3Store::from_lookup(&config(&endpoint, None), credentials("another-secret")).unwrap();
    seen.push_str(&format!("{:?} {wrong:?}", wrong.list(None, 1).await));
    let unreachable = S3Store::from_lookup(&config("http://127.0.0.1:1", None), credentials(SECRET_KEY)).unwrap();
    let failed = unreachable.write(&path("a.txt"), b"x".to_vec(), true).await;
    seen.push_str(&format!("{failed:?} {}", failed.clone().unwrap_err()));
    for secret in [SECRET_KEY, SESSION_TOKEN, "another-secret", "Signature=", "X-Amz-Security-Token"] {
        assert!(!seen.contains(secret), "`{secret}` leaked in: {seen}");
    }
}

#[tokio::test]
async fn plain_http_and_malformed_configuration_are_refused() {
    let lookup = || credentials(SECRET_KEY);
    let base = S3Config::new(BUCKET, REGION);
    let http = S3Config { endpoint: Some("http://minio.internal:9000".into()), ..base.clone() };
    let refused = S3Store::from_lookup(&http, lookup()).unwrap_err().to_string();
    assert!(refused.contains("unencrypted"), "{refused}");
    assert!(S3Store::from_lookup(&S3Config { allow_http: true, ..http }, lookup()).is_ok());
    let invalid = [
        S3Config { endpoint: Some("https://user:pw@s3.example.com".into()), ..base.clone() },
        S3Config { endpoint: Some("https://s3.example.com/extra".into()), ..base.clone() },
        S3Config { bucket: "a/../b".into(), ..base.clone() },
        S3Config { bucket: "Agents?x=1".into(), ..base.clone() },
        S3Config { region: "eu west".into(), ..base.clone() },
        S3Config { prefix: Some("a/../b".into()), ..base.clone() },
        S3Config { prefix: Some("a/.hidden".into()), ..base.clone() },
    ];
    for config in invalid {
        assert!(S3Store::from_lookup(&config, lookup()).is_err(), "{config:?}");
    }
    // AWS itself needs no endpoint, and slashes around a prefix are fine.
    assert!(S3Store::from_lookup(&S3Config { prefix: Some("/agents/support/".into()), ..base }, lookup()).is_ok());
}

#[tokio::test]
async fn throttling_is_retried_and_a_dead_store_is_reported() {
    let (fake, endpoint) = serve().await;
    fake.put("a.txt", b"there");
    let store = store(&endpoint, None);
    fake.fail_first.store(2, Ordering::Relaxed);
    assert_eq!(store.read(&path("a.txt"), 0, 10).await.unwrap(), b"there");
    assert_eq!(fake.log().len(), 3);
    fake.fail_first.store(10, Ordering::Relaxed);
    let failed = store.read(&path("a.txt"), 0, 10).await;
    assert!(matches!(failed, Err(StoreError::Io { path, message }) if path == "a.txt" && message.contains("SlowDown")));
    assert_eq!(fake.log().len(), 6);
}

#[tokio::test]
async fn a_store_that_does_not_answer_times_out() {
    let (fake, endpoint) = serve().await;
    fake.stall.store(true, Ordering::Relaxed);
    let slow = S3Config { request_timeout: Duration::from_millis(150), ..config(&endpoint, None) };
    let store = S3Store::from_lookup(&slow, credentials(SECRET_KEY)).unwrap();
    let started = std::time::Instant::now();
    let failed = store.stat(&path("a.txt")).await;
    assert!(matches!(&failed, Err(StoreError::Io { message, .. }) if message.contains("timed out")), "{failed:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

/// Against a real S3 compatible store, when one is configured:
/// `GRAPHRT_S3_TEST_ENDPOINT`, `GRAPHRT_S3_TEST_BUCKET` and the usual AWS variables.
#[tokio::test]
async fn a_real_object_store_when_one_is_configured() {
    let (Ok(endpoint), Ok(bucket)) =
        (std::env::var("GRAPHRT_S3_TEST_ENDPOINT"), std::env::var("GRAPHRT_S3_TEST_BUCKET"))
    else {
        return;
    };
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into());
    let prefix = format!("graphrt-test-{}", std::process::id());
    let config =
        S3Config { endpoint: Some(endpoint), prefix: Some(prefix), allow_http: true, ..S3Config::new(bucket, region) };
    let store = S3Store::from_env(&config).unwrap();
    let name = path("dir/a &#%+=; é.txt");
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    store.write(&name, bytes.clone(), true).await.unwrap();
    assert!(matches!(store.write(&name, b"x".to_vec(), false).await, Err(StoreError::AlreadyExists(_))));
    let entry = store.stat(&name).await.unwrap().unwrap();
    assert_eq!(entry.size, 300_000);
    assert!(entry.modified_ms.is_some());
    assert_eq!(store.read(&name, 0, 1_000_000).await.unwrap(), bytes);
    assert_eq!(store.read(&name, 299_990, 100).await.unwrap(), &bytes[299_990..]);
    assert_eq!(store.read(&name, 300_000, 100).await.unwrap(), b"");
    assert_eq!(graphrt_files::hex(&store.sha256(&name).await.unwrap()), sigv4::sha256_hex(&bytes));
    let listing = store.list(Some(&path("dir")), 10).await.unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, name);
    store.delete(&name).await.unwrap();
    assert_eq!(store.stat(&name).await.unwrap(), None);
    assert!(store.list(None, 10).await.unwrap().entries.is_empty());
}
