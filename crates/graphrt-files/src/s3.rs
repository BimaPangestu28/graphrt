//! [`FileStore`] over S3 and S3 compatible object stores (cargo feature `s3`).
//!
//! Every file is one object: `<prefix>/<workspace path>` in one bucket. The
//! client is a few signed HTTP requests over `reqwest` (`HeadObject`,
//! `GetObject` with a range, `PutObject`, `DeleteObject`, `ListObjectsV2`),
//! signed with the SigV4 signer of `graphrt-provider-aws`. There is no AWS
//! SDK behind it.
//!
//! What it holds on to, whatever the server answers:
//!
//! - An object key is always the configured prefix plus a [`WorkspacePath`],
//!   percent encoded byte by byte, so no path reaches another prefix, another
//!   bucket or the query string.
//! - A listing only returns keys below the prefix that are valid workspace
//!   paths. Whatever else is in the bucket does not exist for the agent.
//! - No response is read past what was asked for, redirects are not followed
//!   (a signed request must not travel to another host), and every request
//!   has a time limit.
//! - Plain `http://` endpoints need [`S3Config::allow_http`].
//! - Errors and `Debug` output never hold a secret key or a session token.
//!
//! Differences from [`crate::DirStore`], which the [`crate::Workspace`] above
//! the store already covers: deleting an object that is not there succeeds
//! (the workspace looks first), and an object can share its name with a
//! "directory" of other objects.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_provider_aws::auth::sigv4::{self, SigningRequest};
use graphrt_provider_aws::auth::{ChainConfig, Clock, CredentialChain, parse_http_date, parse_iso8601, xml_text};

use crate::path::WorkspacePath;
use crate::store::{Entry, FileStore, Listing, StoreError};

/// Requests per operation before a failure is reported (the first one included).
const MAX_ATTEMPTS: u32 = 3;
/// Wait before the first retry. Doubles for the next one.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
/// Most bytes read of an error answer.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Most bytes read of one page of a listing (1,000 keys of 1 KiB fit four times).
const MAX_LIST_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Keys asked for per page, the most S3 returns.
const LIST_PAGE_KEYS: usize = 1_000;
/// Pages read for one listing, however often the server says there are more.
const MAX_LIST_PAGES: usize = 64;

/// Where the objects are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    /// Bucket name.
    pub bucket: String,
    /// Signing region (`us-east-1`, or whatever the compatible store expects).
    pub region: String,
    /// `https://host[:port]` of an S3 compatible store. `None` is AWS:
    /// `https://<bucket>.s3.<region>.amazonaws.com`.
    pub endpoint: Option<String>,
    /// Key prefix of this store, a relative path such as `agents/support`.
    pub prefix: Option<String>,
    /// Address the bucket in the path (`host/bucket/key`) instead of the host
    /// name. `None`: path style for a custom endpoint, host style for AWS.
    pub path_style: Option<bool>,
    /// Accept an `http://` endpoint. Requests are signed either way, but file
    /// content would travel unencrypted.
    pub allow_http: bool,
    /// Time limit of one request.
    pub request_timeout: Duration,
}

impl S3Config {
    /// A bucket on AWS in `region`, without a prefix.
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            prefix: None,
            path_style: None,
            allow_http: false,
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// Files kept as objects in one bucket. Cheap to clone.
#[derive(Clone)]
pub struct S3Store {
    http: reqwest::Client,
    chain: Arc<CredentialChain>,
    /// `scheme://host[:port]`.
    origin: String,
    /// The signed `host` header.
    host: String,
    /// `/bucket` with path style addressing, else empty.
    bucket_path: String,
    region: String,
    prefix: Option<WorkspacePath>,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("origin", &self.origin)
            .field("bucket_path", &self.bucket_path)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

fn config_error(message: impl fmt::Display) -> StoreError {
    StoreError::Config(message.to_string())
}

/// `scheme` and `host[:port]` of an endpoint that is nothing but an origin.
fn parse_endpoint(endpoint: &str, allow_http: bool) -> Result<(&str, &str), StoreError> {
    let (scheme, rest) =
        endpoint.split_once("://").ok_or_else(|| config_error("the endpoint must start with https:// (or http://)"))?;
    match scheme {
        "https" => {}
        "http" if allow_http => {}
        "http" => {
            return Err(config_error("an http:// endpoint sends files unencrypted and must be allowed explicitly"));
        }
        _ => return Err(config_error("the endpoint must start with https:// (or http://)")),
    }
    let host = rest.strip_suffix('/').unwrap_or(rest);
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
    if host.is_empty() || !host.chars().all(plain) {
        return Err(config_error("the endpoint must be scheme://host[:port], without a path, a query or credentials"));
    }
    Ok((scheme, host))
}

/// Bucket names are DNS labels: nothing in them can change the shape of a URL.
fn check_bucket(bucket: &str) -> Result<(), StoreError> {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-');
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid = (3..=63).contains(&bucket.len())
        && bucket.chars().all(plain)
        && edge(bucket.chars().next())
        && edge(bucket.chars().last())
        && !bucket.contains("..");
    if valid { Ok(()) } else { Err(config_error(format!("`{bucket}` is not a valid bucket name"))) }
}

impl S3Store {
    /// A store for `config`, signing with the credentials `chain` resolves.
    ///
    /// @throws StoreError - An invalid bucket, endpoint or prefix, or an HTTP client that cannot be built.
    pub fn new(config: &S3Config, chain: Arc<CredentialChain>) -> Result<Self, StoreError> {
        check_bucket(&config.bucket)?;
        let region_ok =
            !config.region.is_empty() && config.region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !region_ok {
            return Err(config_error(format!("`{}` is not a valid region", config.region)));
        }
        let prefix = match config.prefix.as_deref().map(|prefix| prefix.trim_matches('/')).filter(|p| !p.is_empty()) {
            Some(prefix) => {
                Some(WorkspacePath::parse(prefix).map_err(|e| config_error(format!("prefix `{prefix}`: {e}")))?)
            }
            None => None,
        };
        let (origin, host, bucket_path) = match &config.endpoint {
            Some(endpoint) => {
                let (scheme, host) = parse_endpoint(endpoint, config.allow_http)?;
                if config.path_style.unwrap_or(true) {
                    (format!("{scheme}://{host}"), host.to_owned(), format!("/{}", config.bucket))
                } else {
                    let host = format!("{}.{host}", config.bucket);
                    (format!("{scheme}://{host}"), host, String::new())
                }
            }
            None if config.path_style == Some(true) => {
                let host = format!("s3.{}.amazonaws.com", config.region);
                (format!("https://{host}"), host, format!("/{}", config.bucket))
            }
            None => {
                let host = format!("{}.s3.{}.amazonaws.com", config.bucket, config.region);
                (format!("https://{host}"), host, String::new())
            }
        };
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(config.request_timeout)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| config_error(format!("cannot build the HTTP client: {e}")))?;
        Ok(Self { http, chain, origin, host, bucket_path, region: config.region.clone(), prefix })
    }

    /// A store for `config` with credentials from the usual AWS variables,
    /// read through `lookup`: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
    /// (with `AWS_SESSION_TOKEN`), else the container, web identity and
    /// instance role sources of the credential chain.
    ///
    /// @throws StoreError - As [`S3Store::new`], or a credential chain that cannot be built.
    pub fn from_lookup(config: &S3Config, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError> {
        let chain = CredentialChain::new(ChainConfig::from_lookup(lookup), Clock::default())
            .map_err(|e| config_error(e.to_string()))?;
        Self::new(config, Arc::new(chain))
    }

    /// [`S3Store::from_lookup`] over the process environment.
    ///
    /// @throws StoreError - As [`S3Store::from_lookup`].
    pub fn from_env(config: &S3Config) -> Result<Self, StoreError> {
        Self::from_lookup(config, |name| std::env::var(name).ok())
    }

    /// The object key of a workspace path.
    fn key(&self, path: &WorkspacePath) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}/{path}"),
            None => path.to_string(),
        }
    }

    /// The request path of an object: every byte of the key that is not
    /// unreserved is percent encoded, so a key is never read as URL syntax.
    fn object_path(&self, path: &WorkspacePath) -> String {
        format!("{}/{}", self.bucket_path, sigv4::uri_encode(&self.key(path), true))
    }

    /// The request path of the bucket itself.
    fn bucket_root(&self) -> String {
        if self.bucket_path.is_empty() { "/".to_owned() } else { self.bucket_path.clone() }
    }

    /// Sign and send one request once.
    async fn send_once(&self, request: &Request<'_>) -> Result<reqwest::Response, Failure> {
        let credentials = self.chain.credentials().await.map_err(|e| Failure::Fatal(e.to_string()))?;
        let date = sigv4::amz_date(self.chain.clock().now_unix());
        let mut headers: Vec<(&str, &str)> =
            vec![("host", &self.host), ("x-amz-content-sha256", request.payload_hash), ("x-amz-date", &date)];
        if let Some(token) = &credentials.session_token {
            headers.push(("x-amz-security-token", token));
        }
        headers.extend_from_slice(request.headers);
        let signing = SigningRequest {
            method: request.method.as_str(),
            path: request.path,
            query: request.query,
            headers: &headers,
            payload_hash: request.payload_hash,
            region: &self.region,
            service: "s3",
            amz_date: &date,
            // S3 signs the path as it is sent.
            double_encode_path: false,
        };
        let authorization = sigv4::authorization(&credentials, &signing);
        let mut url = format!("{}{}", self.origin, request.path);
        for (index, (name, value)) in request.query.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(&sigv4::uri_encode(name, false));
            url.push('=');
            url.push_str(&sigv4::uri_encode(value, false));
        }
        let mut builder = self.http.request(request.method.clone(), url).header("authorization", authorization);
        for (name, value) in headers.iter().filter(|(name, _)| *name != "host") {
            builder = builder.header(*name, *value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        builder.send().await.map_err(|e| Failure::Transient(describe_transport(&e)))
    }

    /// Send `request` and return the answer when its status is a success or
    /// one of `expected`. Throttling, server errors and broken connections
    /// are retried. A clock that is off and credentials that expired are each
    /// corrected once.
    async fn send(&self, request: &Request<'_>, expected: &[u16]) -> Result<reqwest::Response, StoreError> {
        let (mut corrected_clock, mut refreshed_credentials) = (false, false);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let failure = match self.send_once(request).await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if response.status().is_success() || expected.contains(&status) {
                        return Ok(response);
                    }
                    let server_date = header(&response, "date").and_then(|date| parse_http_date(&date));
                    let error = S3Error::read(response).await;
                    if let Some(now) = server_date.filter(|_| error.is_clock_skew() && !corrected_clock) {
                        self.chain.clock().correct_to(now);
                        corrected_clock = true;
                        continue;
                    }
                    if error.is_stale_credentials() && !refreshed_credentials {
                        self.chain.invalidate().await;
                        refreshed_credentials = true;
                        continue;
                    }
                    if error.is_transient() { Failure::Transient(error.summary()) } else { Failure::Status(error) }
                }
                Err(failure) => failure,
            };
            match failure {
                Failure::Transient(_) if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(RETRY_BASE_DELAY.saturating_mul(1 << (attempt - 1))).await;
                }
                Failure::Transient(message) | Failure::Fatal(message) => {
                    return Err(StoreError::Io { path: request.shown.to_owned(), message });
                }
                Failure::Status(error) => return Err(error.into_store_error(request.shown)),
            }
        }
    }
}

/// One request before it is signed.
struct Request<'a> {
    method: reqwest::Method,
    /// Path as sent (percent encoded).
    path: &'a str,
    /// Query parameters, not encoded.
    query: &'a [(&'a str, &'a str)],
    /// Extra signed headers, names in lower case.
    headers: &'a [(&'a str, &'a str)],
    body: Option<bytes::Bytes>,
    /// Hex SHA-256 of the body.
    payload_hash: &'a str,
    /// The path errors name: the workspace path, never the object key.
    shown: &'a str,
}

impl<'a> Request<'a> {
    fn new(method: reqwest::Method, path: &'a str, shown: &'a str) -> Self {
        Self { method, path, query: &[], headers: &[], body: None, payload_hash: sigv4::EMPTY_PAYLOAD_SHA256, shown }
    }
}

/// Why a request did not produce a usable answer.
enum Failure {
    /// Worth another attempt.
    Transient(String),
    /// Not worth one.
    Fatal(String),
    /// The store answered with an error.
    Status(S3Error),
}

/// What went wrong with a connection, without the URL (it can hold a key name).
fn describe_transport(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the request to the object store timed out".to_owned()
    } else if error.is_connect() {
        "cannot connect to the object store".to_owned()
    } else {
        "the request to the object store failed".to_owned()
    }
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned)
}

/// Up to `max_bytes` of a response body. `None` when the body is larger and `strict`.
async fn read_body(mut response: reqwest::Response, max_bytes: usize, strict: bool) -> Result<Option<Vec<u8>>, String> {
    let announced = response.content_length().and_then(|length| usize::try_from(length).ok());
    let mut body = Vec::with_capacity(announced.unwrap_or(0).min(max_bytes));
    while let Some(piece) = response.chunk().await.map_err(|e| describe_transport(&e))? {
        let room = max_bytes.saturating_sub(body.len());
        if piece.len() > room {
            if strict {
                return Ok(None);
            }
            body.extend_from_slice(piece.get(..room).unwrap_or_default());
            return Ok(Some(body));
        }
        body.extend_from_slice(&piece);
    }
    Ok(Some(body))
}

/// An error answer of the store.
#[derive(Debug)]
struct S3Error {
    status: u16,
    /// `NoSuchKey`, `AccessDenied`... Empty when the answer had no body (`HEAD`).
    code: String,
    message: String,
}

impl S3Error {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let body = read_body(response, MAX_ERROR_BODY_BYTES, false).await.ok().flatten().unwrap_or_default();
        let body = String::from_utf8_lossy(&body);
        let text = |tag: &str| xml_text(&body, tag).unwrap_or_default();
        // One line, bounded: this text ends up in tool results and logs.
        let message: String = text("Message").chars().filter(|c| !c.is_control()).take(200).collect();
        Self { status, code: text("Code").chars().filter(char::is_ascii_alphanumeric).take(64).collect(), message }
    }

    fn is_clock_skew(&self) -> bool {
        matches!(self.code.as_str(), "RequestTimeTooSkewed" | "RequestExpired")
    }

    fn is_stale_credentials(&self) -> bool {
        matches!(self.code.as_str(), "ExpiredToken" | "ExpiredTokenException" | "TokenRefreshRequired")
    }

    fn is_transient(&self) -> bool {
        self.status >= 500 || self.status == 429 || matches!(self.code.as_str(), "SlowDown" | "RequestTimeout")
    }

    fn summary(&self) -> String {
        match (self.code.is_empty(), self.message.is_empty()) {
            (true, _) => format!("the object store answered HTTP {}", self.status),
            (false, true) => format!("the object store answered {} (HTTP {})", self.code, self.status),
            (false, false) => {
                format!("the object store answered {} (HTTP {}): {}", self.code, self.status, self.message)
            }
        }
    }

    fn into_store_error(self, shown: &str) -> StoreError {
        match self.status {
            404 if self.code != "NoSuchBucket" => StoreError::NotFound(shown.to_owned()),
            401 | 403 => StoreError::Denied { path: shown.to_owned(), reason: self.summary() },
            301 | 302 | 307 | 308 => StoreError::Io {
                path: shown.to_owned(),
                message: "the object store redirected the request, which is not followed: check the region and the \
                          endpoint"
                    .to_owned(),
            },
            _ => StoreError::Io { path: shown.to_owned(), message: self.summary() },
        }
    }
}

/// One `<Contents>` element of a listing.
#[derive(Debug, PartialEq, Eq)]
struct ListedObject {
    key: String,
    size: u64,
    modified_ms: Option<u64>,
}

/// One page of `ListObjectsV2`.
#[derive(Debug, Default, PartialEq, Eq)]
struct ListPage {
    objects: Vec<ListedObject>,
    /// Token of the next page, when the store says there is one.
    next: Option<String>,
}

/// Read a `ListObjectsV2` answer. A deliberately small reader for a flat,
/// known shape: it expands the five XML entities and nothing else, so a
/// document cannot make it allocate more than its own size.
fn parse_list_page(xml: &str) -> ListPage {
    let mut page = ListPage::default();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = rest.get(start + "<Contents>".len()..).unwrap_or_default();
        let Some(end) = after.find("</Contents>") else { break };
        let element = after.get(..end).unwrap_or_default();
        rest = after.get(end..).unwrap_or_default();
        let Some(key) = xml_text(element, "Key") else { continue };
        let Some(size) = xml_text(element, "Size").and_then(|size| size.parse().ok()) else { continue };
        let modified_ms = xml_text(element, "LastModified")
            .and_then(|stamp| parse_iso8601(&stamp))
            .map(|seconds| seconds.saturating_mul(1000));
        page.objects.push(ListedObject { key, size, modified_ms });
    }
    if xml_text(xml, "IsTruncated").as_deref() == Some("true") {
        page.next = xml_text(xml, "NextContinuationToken").filter(|token| !token.is_empty());
    }
    page
}

impl FileStore for S3Store {
    fn stat<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<Option<Entry>, StoreError>> {
        Box::pin(async move {
            let object = self.object_path(path);
            let request = Request::new(reqwest::Method::HEAD, &object, path.as_str());
            let response = self.send(&request, &[404]).await?;
            if response.status().as_u16() == 404 {
                return Ok(None);
            }
            let size = header(&response, "content-length").and_then(|length| length.parse().ok()).unwrap_or(0);
            let modified_ms = header(&response, "last-modified")
                .and_then(|date| parse_http_date(&date))
                .map(|seconds| seconds.saturating_mul(1000));
            Ok(Some(Entry { path: path.clone(), size, modified_ms }))
        })
    }

    fn read<'a>(
        &'a self,
        path: &'a WorkspacePath,
        offset: u64,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        Box::pin(async move {
            if max_bytes == 0 {
                // Nothing to fetch, but a missing file is still an error.
                let found = self.stat(path).await?;
                return found.map(|_| Vec::new()).ok_or_else(|| StoreError::NotFound(path.to_string()));
            }
            let object = self.object_path(path);
            let range = format!("bytes={offset}-{}", offset.saturating_add(max_bytes - 1));
            let headers = [("range", range.as_str())];
            let request = Request { headers: &headers, ..Request::new(reqwest::Method::GET, &object, path.as_str()) };
            // 416: the range starts at or after the end of the object.
            let response = self.send(&request, &[416]).await?;
            let status = response.status().as_u16();
            if status == 416 {
                return Ok(Vec::new());
            }
            if status != 206 && offset > 0 {
                return Err(StoreError::io(path.as_str(), "the object store ignored the requested byte range"));
            }
            // Never more than was asked for, whatever the store sends.
            let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
            let body = read_body(response, limit, false).await.map_err(|e| StoreError::io(path.as_str(), e))?;
            Ok(body.unwrap_or_default())
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a WorkspacePath,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'a, Result<Entry, StoreError>> {
        Box::pin(async move {
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            // The hash is part of the signature, so the store rejects a body damaged on the way.
            let hashed = crate::blocking(move || (sigv4::sha256_hex(&bytes), bytes::Bytes::from(bytes))).await;
            let (payload_hash, body) = hashed.map_err(|e| StoreError::io(path.as_str(), e))?;
            let object = self.object_path(path);
            // Without `overwrite` the store itself refuses to replace an object,
            // also one another process created a moment ago.
            let conditional = [("if-none-match", "*")];
            let request = Request {
                headers: if overwrite { &[] } else { &conditional },
                body: Some(body),
                payload_hash: &payload_hash,
                ..Request::new(reqwest::Method::PUT, &object, path.as_str())
            };
            let response = self.send(&request, &[409, 412]).await?;
            if matches!(response.status().as_u16(), 409 | 412) {
                return Err(StoreError::AlreadyExists(path.to_string()));
            }
            Ok(Entry { path: path.clone(), size, modified_ms: None })
        })
    }

    fn delete<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let object = self.object_path(path);
            let request = Request::new(reqwest::Method::DELETE, &object, path.as_str());
            self.send(&request, &[]).await.map(|_| ())
        })
    }

    fn list<'a>(
        &'a self,
        dir: Option<&'a WorkspacePath>,
        max_entries: usize,
    ) -> BoxFuture<'a, Result<Listing, StoreError>> {
        Box::pin(async move {
            let shown = dir.map(WorkspacePath::as_str).unwrap_or_default();
            // Keys start with `<prefix>/`, and with `<prefix>/<dir>/` below a directory.
            let store_prefix = self.prefix.as_ref().map(|prefix| format!("{prefix}/")).unwrap_or_default();
            let wanted = match dir {
                Some(dir) => format!("{store_prefix}{dir}/"),
                None => store_prefix.clone(),
            };
            let root = self.bucket_root();
            let page_keys = LIST_PAGE_KEYS.to_string();
            let mut listing = Listing::default();
            let mut token: Option<String> = None;
            for _ in 0..MAX_LIST_PAGES {
                let mut query = vec![("list-type", "2"), ("max-keys", page_keys.as_str()), ("prefix", wanted.as_str())];
                if let Some(token) = &token {
                    query.push(("continuation-token", token.as_str()));
                }
                let request = Request { query: &query, ..Request::new(reqwest::Method::GET, &root, shown) };
                let response = self.send(&request, &[]).await?;
                let body =
                    read_body(response, MAX_LIST_BODY_BYTES, true).await.map_err(|e| StoreError::io(shown, e))?;
                let body = body.ok_or_else(|| StoreError::io(shown, "the listing of the object store is too large"))?;
                let page = parse_list_page(&String::from_utf8_lossy(&body));
                for object in page.objects {
                    // Only keys below the asked prefix that are workspace paths exist here.
                    let Some(path) = object
                        .key
                        .strip_prefix(&store_prefix)
                        .filter(|_| object.key.starts_with(&wanted))
                        .and_then(|relative| WorkspacePath::parse(relative).ok())
                    else {
                        continue;
                    };
                    if listing.entries.len() >= max_entries {
                        listing.truncated = true;
                        return Ok(listing);
                    }
                    listing.entries.push(Entry { path, size: object.size, modified_ms: object.modified_ms });
                }
                token = page.next;
                if token.is_none() {
                    listing.entries.sort_by(|a, b| a.path.cmp(&b.path));
                    return Ok(listing);
                }
            }
            // The store kept announcing pages: report what was read as incomplete.
            listing.truncated = true;
            Ok(listing)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_origins_and_http_needs_a_switch() {
        assert_eq!(parse_endpoint("https://minio.internal:9000", false).unwrap(), ("https", "minio.internal:9000"));
        assert_eq!(parse_endpoint("https://s3.example.com/", false).unwrap(), ("https", "s3.example.com"));
        assert_eq!(parse_endpoint("http://127.0.0.1:9000", true).unwrap(), ("http", "127.0.0.1:9000"));
        assert_eq!(parse_endpoint("https://[::1]:9000", false).unwrap(), ("https", "[::1]:9000"));
        let refused = [
            "http://127.0.0.1:9000",
            "s3.example.com",
            "ftp://s3.example.com",
            "https://",
            "https://user:secret@s3.example.com",
            "https://s3.example.com/path",
            "https://s3.example.com?x=1",
            "https://s3.example.com#frag",
            "https://s3 .example.com",
            "https://s3.example.com\r\nHost: evil",
        ];
        for endpoint in refused {
            assert!(parse_endpoint(endpoint, false).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn bucket_names_cannot_carry_url_syntax() {
        for bucket in ["agents", "my-bucket.files", "a1b"] {
            assert!(check_bucket(bucket).is_ok(), "{bucket}");
        }
        for bucket in ["ab", "Agents", "a/b", "a..b", "-ab", "ab-", "a?b=c", "a b", "a%2fb", "a@b", ""] {
            assert!(check_bucket(bucket).is_err(), "{bucket}");
        }
    }

    #[test]
    fn a_listing_is_read_without_expanding_anything() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>true</IsTruncated>\
            <Contents><Key>p/a &amp; b.txt</Key><LastModified>2026-11-27T01:02:03.000Z</LastModified>\
            <Size>12</Size></Contents>\
            <Contents><Key>p/no-size.txt</Key></Contents>\
            <Contents><Key>p/c.txt</Key><Size>3</Size></Contents>\
            <NextContinuationToken>tok&lt;1&gt;</NextContinuationToken></ListBucketResult>";
        let page = parse_list_page(xml);
        assert_eq!(page.next.as_deref(), Some("tok<1>"));
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "p/a & b.txt");
        assert_eq!(page.objects[0].size, 12);
        assert_eq!(page.objects[0].modified_ms, Some(1_795_741_323_000));
        assert_eq!(page.objects[1], ListedObject { key: "p/c.txt".into(), size: 3, modified_ms: None });
        // An entity that is not one of the five stays text, and a cut document ends the page.
        let odd = parse_list_page("<Contents><Key>&xxe;</Key><Size>1</Size></Contents><Contents><Key>cut");
        assert_eq!(odd.objects.len(), 1);
        assert_eq!(odd.objects[0].key, "&xxe;");
        assert_eq!(odd.next, None);
    }
}
