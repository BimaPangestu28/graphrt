//! `/v1/files`: the agents' workspace over HTTP (`--workspace-api`).
//!
//! A chat client uploads what a person attached, lists what the agent wrote
//! and downloads it. The routes are off unless `--workspace-api` is given, and
//! everything they do goes through the same [`Workspace`] the `files` tools
//! use, so paths, quotas, type checks and user scopes are the ones the agent
//! lives under.
//!
//! | Route | Does |
//! | --- | --- |
//! | `GET /v1/files?dir=` | list files |
//! | `GET /v1/files/info?path=` | size, detected type, SHA-256 |
//! | `GET /v1/files/content?path=` | download |
//! | `PUT /v1/files/content?path=&overwrite=` | upload (raw body) |
//! | `DELETE /v1/files/content?path=` | delete |
//!
//! What a client sends is never trusted:
//!
//! - **Paths** travel in the query string, not in the URL path, so no proxy
//!   or client library can normalise `a/../b` into another file on the way.
//!   They are parsed as [`WorkspacePath`] before storage is touched.
//! - **Who** decides **where**: a verified end user (`--workspace-api users`)
//!   only ever reaches the directory of their own `sub`. Only the API key can
//!   name a `scope`.
//! - **Uploads** are counted while they arrive and stop at the limit. A body
//!   that stalls is dropped, and only a few uploads are held in memory at once.
//! - **Downloads** always carry `nosniff` and a sandboxing
//!   `Content-Security-Policy`. HTML, SVG and programs are sent as
//!   `application/octet-stream` attachments whatever the client asks for.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use graphrt_core::Value;
use graphrt_files::{FilesError, MediaClass, MediaType, StoreError, StoredFile, Workspace, WorkspacePath};
use serde_json::json;

use super::auth::Caller;
use super::errors::error_response;
use super::state::AppState;
use crate::args::WorkspaceApi;
use crate::error::CliError;
use crate::http_input::QueryParams;
use crate::setup::ServedWorkspace;

/// Uploads held in memory at the same time. More wait for a turn.
const MAX_CONCURRENT_UPLOADS: usize = 4;
/// Longest pause between two pieces of an upload.
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest time one upload may take.
const UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
/// Bytes read from storage per step of a download.
const DOWNLOAD_CHUNK_BYTES: u64 = 256 * 1024;
/// Most files one listing returns.
const MAX_LIST_ENTRIES: usize = 1_000;
/// Most files one run may attach (`files` of a run request).
pub(crate) const MAX_RUN_ATTACHMENTS: usize = 16;

/// The workspace routes and who may use them.
#[derive(Debug)]
pub(crate) struct FileApi {
    workspace: Workspace,
    per_user: bool,
    /// `--workspace-api users`: verified end users reach their own directory.
    users: bool,
    max_upload_bytes: u64,
    uploads: tokio::sync::Semaphore,
}

impl FileApi {
    /// The routes over `served`, for `who`.
    ///
    /// @throws CliError (usage) - `users` without a workspace per user or without `--verify-jwt`.
    pub(crate) fn new(
        served: ServedWorkspace,
        who: WorkspaceApi,
        verifies_user_tokens: bool,
        max_upload_bytes: usize,
    ) -> Result<Self, CliError> {
        let users = who == WorkspaceApi::Users;
        if users && !served.per_user {
            return Err(CliError::usage(
                "`--workspace-api users` needs `--workspace-per-user`: end users must not share one directory",
            ));
        }
        if users && !verifies_user_tokens {
            return Err(CliError::usage(
                "`--workspace-api users` needs `--verify-jwt`: a user is known by a verified token only",
            ));
        }
        let max_file_bytes = served.workspace.limits().max_file_bytes;
        Ok(Self {
            workspace: served.workspace,
            per_user: served.per_user,
            users,
            max_upload_bytes: u64::try_from(max_upload_bytes).unwrap_or(u64::MAX).min(max_file_bytes),
            uploads: tokio::sync::Semaphore::new(MAX_CONCURRENT_UPLOADS),
        })
    }

    /// One line for the startup banner.
    pub(crate) fn describe(&self) -> String {
        let who = if self.users { "the API key and verified users" } else { "the API key" };
        format!("workspace API under /v1/files for {who} (uploads up to {} bytes)", self.max_upload_bytes)
    }

    /// The part of the workspace `caller` works in.
    ///
    /// @param scope - The `scope` a request named, which only the API key may do.
    /// @throws Response - `400` for a missing or misplaced scope, `403` for a user token on a key-only API.
    fn view(&self, caller: &Caller, scope: Option<&str>) -> Result<Workspace, Response> {
        match caller {
            #[cfg(feature = "jwt")]
            Caller::User(subject) => {
                if !self.users {
                    return Err(error_response(StatusCode::FORBIDDEN, "the workspace API does not accept user tokens"));
                }
                if scope.is_some() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "`scope` is taken from the user token and cannot be sent with one",
                    ));
                }
                Ok(self.workspace.scoped(subject))
            }
            Caller::ApiKey => match (self.per_user, scope.filter(|scope| !scope.is_empty())) {
                (true, Some(scope)) => Ok(self.workspace.scoped(scope)),
                (true, None) => Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "this workspace has one directory per user: send `scope` (the user's id)",
                )),
                (false, Some(_)) => Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "`scope` needs a workspace with one directory per user (`--workspace-per-user`)",
                )),
                (false, None) => Ok(self.workspace.clone()),
            },
        }
    }

    /// The view and path a `workspace:` reference names, as far as `caller` may see it.
    fn locate(&self, caller: &Caller, reference: &str) -> Option<(Workspace, WorkspacePath)> {
        match caller {
            #[cfg(feature = "jwt")]
            Caller::User(_) => {
                let view = self.view(caller, None).ok()?;
                let path = view.path_of_reference(reference)?;
                Some((view, path))
            }
            Caller::ApiKey => self.workspace.locate_reference(reference),
        }
    }

    /// The file a request names with `path` (and `scope`) or with `ref`.
    ///
    /// @throws Response - `400` for a bad path or parameters, `404` for a reference out of the caller's reach.
    fn target(&self, caller: &Caller, params: &QueryParams) -> Result<(Workspace, WorkspacePath), Response> {
        match (params.get("path"), params.get("ref")) {
            (Some(_), Some(_)) => Err(error_response(StatusCode::BAD_REQUEST, "send `path` or `ref`, not both")),
            (Some(path), None) => {
                let view = self.view(caller, params.get("scope"))?;
                Ok((view, parse_path(path)?))
            }
            // A reference of another user's file answers like a file that is not there.
            (None, Some(reference)) => self
                .locate(caller, reference)
                .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "no such file in this workspace")),
            (None, None) => Err(error_response(StatusCode::BAD_REQUEST, "missing `path`")),
        }
    }

    /// The file values of the workspace files a run request attached (their
    /// `workspace:` references), for `sys.files` of the run.
    ///
    /// @throws String - A message for a `400` response: too many, not a file the caller can reach, or missing.
    pub(crate) async fn attachments(&self, caller: &Caller, references: &[String]) -> Result<Value, String> {
        if references.len() > MAX_RUN_ATTACHMENTS {
            return Err(format!("`files` has more than {MAX_RUN_ATTACHMENTS} entries"));
        }
        let mut files = Vec::with_capacity(references.len());
        for reference in references {
            let missing = || format!("`files`: `{reference}` is not a file of this workspace");
            let (view, path) = self.locate(caller, reference).ok_or_else(missing)?;
            let stored = view.inspect(&path).await.map_err(|_| missing())?;
            files.push(Value::File(Arc::new(view.file_value(&stored))));
        }
        Ok(Value::Array(Arc::new(files)))
    }
}

/// The `/v1/files` routes. They authenticate themselves, because a verified
/// end user may call them without the API key.
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/files", get(list_files))
        .route("/v1/files/info", get(file_info))
        .route("/v1/files/content", get(download_file).put(upload_file).delete(delete_file))
}

/// Who is calling: a verified end user where the API accepts them, else the API key.
///
/// @throws Response - `401` for an invalid token or key.
async fn authenticate(state: &AppState, api: &FileApi, headers: &HeaderMap) -> Result<Caller, Response> {
    #[cfg(feature = "jwt")]
    if api.users
        && let Some(user_auth) = &state.user_auth
        && let crate::user_auth::Admission::User(user) = user_auth.admit_unmetered(headers).await?
    {
        return Ok(Caller::User(user.subject));
    }
    #[cfg(not(feature = "jwt"))]
    let _ = api;
    match state.refuse_without_api_key(headers) {
        Some(refused) => Err(refused),
        None => Ok(Caller::ApiKey),
    }
}

/// The API, the caller and the query of a request, or the response that refuses it.
async fn admitted<'a>(
    state: &'a AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
) -> Result<(&'a FileApi, Caller, QueryParams), Response> {
    let Some(api) = state.file_api.as_deref() else {
        return Err(error_response(StatusCode::NOT_FOUND, "the workspace API is not served"));
    };
    let caller = authenticate(state, api, headers).await?;
    Ok((api, caller, QueryParams::parse(raw_query)))
}

fn parse_path(raw: &str) -> Result<WorkspacePath, Response> {
    WorkspacePath::parse(raw).map_err(|e| error_response(StatusCode::BAD_REQUEST, &format!("invalid path: {e}")))
}

/// The status and message of a refused workspace operation. Storage failures
/// name server paths, so they are logged and answered without detail.
fn files_error(error: &FilesError) -> Response {
    let status = match error {
        FilesError::Path(_) => StatusCode::BAD_REQUEST,
        FilesError::Store(StoreError::NotFound(_)) => StatusCode::NOT_FOUND,
        FilesError::Store(StoreError::AlreadyExists(_)) => StatusCode::CONFLICT,
        FilesError::Store(StoreError::NotAFile(_)) => StatusCode::BAD_REQUEST,
        FilesError::Store(StoreError::Denied { .. }) | FilesError::ReadOnly => StatusCode::FORBIDDEN,
        FilesError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        FilesError::QuotaExceeded { .. } | FilesError::TooManyFiles { .. } => StatusCode::INSUFFICIENT_STORAGE,
        FilesError::Refused { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        _ => {
            eprintln!("workspace storage error: {error}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "workspace storage error (details in the server log)",
            );
        }
    };
    error_response(status, &error.to_string())
}

/// What a client learns about a stored file. `file` is the Graphrt file value:
/// its `reference` names the file in a run request (`files`) and in `?ref=`.
fn describe_file(view: &Workspace, stored: &StoredFile) -> serde_json::Value {
    json!({
        "path": stored.entry.path.as_str(),
        "size": stored.entry.size,
        "modified_ms": stored.entry.modified_ms,
        "mime_type": stored.media.mime,
        "inline_safe": stored.media.is_inline_safe(),
        "file": view.file_value(stored),
    })
}

/// `GET /v1/files?dir=&scope=`: the files below a directory, or all of them.
async fn list_files(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, headers: HeaderMap) -> Response {
    let (api, caller, params) = match admitted(&state, &headers, raw.as_deref()).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let view = match api.view(&caller, params.get("scope")) {
        Ok(view) => view,
        Err(refused) => return refused,
    };
    let dir = match params.get("dir").filter(|dir| !dir.is_empty()).map(parse_path).transpose() {
        Ok(dir) => dir,
        Err(refused) => return refused,
    };
    let listing = match view.list(dir.as_ref(), MAX_LIST_ENTRIES).await {
        Ok(listing) => listing,
        Err(e) => return files_error(&e),
    };
    let usage = match view.usage().await {
        Ok(usage) => usage,
        Err(e) => return files_error(&e),
    };
    let files: Vec<serde_json::Value> = listing
        .entries
        .iter()
        .map(|entry| json!({ "path": entry.path.as_str(), "size": entry.size, "modified_ms": entry.modified_ms }))
        .collect();
    let limits = view.limits();
    Json(json!({
        "files": files,
        "truncated": listing.truncated,
        "usage": { "files": usage.files, "bytes": usage.bytes },
        "limits": {
            "max_file_bytes": limits.max_file_bytes.min(api.max_upload_bytes),
            "max_total_bytes": limits.max_total_bytes,
            "max_files": limits.max_files,
            "read_only": limits.read_only,
        },
    }))
    .into_response()
}

/// `GET /v1/files/info?path=`: size, detected type and SHA-256 of one file.
async fn file_info(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, headers: HeaderMap) -> Response {
    let (api, caller, params) = match admitted(&state, &headers, raw.as_deref()).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let (view, path) = match api.target(&caller, &params) {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    let stored = match view.inspect(&path).await {
        Ok(stored) => stored,
        Err(e) => return files_error(&e),
    };
    let sha256 = match view.sha256(&path).await {
        Ok(sha256) => sha256,
        Err(e) => return files_error(&e),
    };
    let mut body = describe_file(&view, &stored);
    if let Some(object) = body.as_object_mut() {
        object.insert("sha256".to_owned(), json!(sha256));
    }
    Json(body).into_response()
}

/// `DELETE /v1/files/content?path=`: remove one file.
async fn delete_file(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, headers: HeaderMap) -> Response {
    let (api, caller, params) = match admitted(&state, &headers, raw.as_deref()).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let (view, path) = match api.target(&caller, &params) {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    match view.delete(&path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => files_error(&e),
    }
}

/// `PUT /v1/files/content?path=&overwrite=`: store the request body as a file.
async fn upload_file(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, request: Request) -> Response {
    // Authorized before any byte of the body is read.
    let (api, caller, params) = match admitted(&state, request.headers(), raw.as_deref()).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let (view, path) = match api.target(&caller, &params) {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    let overwrite = match params.boolean("overwrite") {
        Ok(overwrite) => overwrite.unwrap_or(false),
        Err(refused) => return refused,
    };
    if view.limits().read_only {
        return files_error(&FilesError::ReadOnly);
    }
    let limit = api.max_upload_bytes;
    let declared = request.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|declared| declared > limit) {
        return upload_error(&UploadError::TooLarge { limit });
    }
    // A name that is taken is refused before the body is read, not after.
    if !overwrite && matches!(view.stat(&path).await, Ok(Some(_))) {
        return files_error(&StoreError::AlreadyExists(path.to_string()).into());
    }
    let Ok(_turn) = api.uploads.acquire().await else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down");
    };
    let reading = read_upload(request.into_body(), limit, UPLOAD_IDLE_TIMEOUT);
    let bytes = match tokio::time::timeout(UPLOAD_TOTAL_TIMEOUT, reading).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return upload_error(&e),
        Err(_) => return upload_error(&UploadError::Stalled),
    };
    if bytes.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "the request body (the file) is empty");
    }
    match view.write(&path, bytes, overwrite).await {
        Ok(stored) => (StatusCode::CREATED, Json(describe_file(&view, &stored))).into_response(),
        Err(e) => files_error(&e),
    }
}

/// Why an upload was not read to its end.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum UploadError {
    #[error("the upload is larger than the limit of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("the upload stalled")]
    Stalled,
    #[error("the upload was interrupted")]
    Interrupted,
}

fn upload_error(error: &UploadError) -> Response {
    let status = match error {
        UploadError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        UploadError::Stalled => StatusCode::REQUEST_TIMEOUT,
        UploadError::Interrupted => StatusCode::BAD_REQUEST,
    };
    error_response(status, &error.to_string())
}

/// Read a request body, giving up as soon as it passes `limit` bytes or
/// pauses for longer than `idle`. Nothing above the limit is ever buffered.
async fn read_upload(body: Body, limit: u64, idle: Duration) -> Result<Vec<u8>, UploadError> {
    let mut stream = body.into_data_stream();
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let piece = match tokio::time::timeout(idle, stream.next()).await {
            Err(_) => return Err(UploadError::Stalled),
            Ok(None) => return Ok(bytes),
            Ok(Some(Err(_))) => return Err(UploadError::Interrupted),
            Ok(Some(Ok(piece))) => piece,
        };
        let total = bytes.len().saturating_add(piece.len());
        if u64::try_from(total).unwrap_or(u64::MAX) > limit {
            return Err(UploadError::TooLarge { limit });
        }
        bytes.extend_from_slice(&piece);
    }
}

/// `GET /v1/files/content?path=&inline=`: the bytes of one file.
async fn download_file(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, headers: HeaderMap) -> Response {
    let (api, caller, params) = match admitted(&state, &headers, raw.as_deref()).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let (view, path) = match api.target(&caller, &params) {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    let inline = match params.boolean("inline") {
        Ok(inline) => inline.unwrap_or(false),
        Err(refused) => return refused,
    };
    let stored = match view.inspect(&path).await {
        Ok(stored) => stored,
        Err(e) => return files_error(&e),
    };
    let size = stored.entry.size;
    let mut response = Response::new(Body::from_stream(file_stream(view, path.clone(), size)));
    let delivery = Delivery::of(stored.media, inline);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(delivery.content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(disposition) = HeaderValue::from_str(&content_disposition(delivery.inline, path.file_name())) {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    // The type above is final: no browser may guess another one from the bytes.
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Whatever the file is, it runs no script, loads nothing and frames nothing.
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    headers.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// The file in steps of [`DOWNLOAD_CHUNK_BYTES`], so a download holds one step in memory.
fn file_stream(
    view: Workspace,
    path: WorkspacePath,
    size: u64,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
    futures::stream::unfold(0u64, move |offset| {
        let (view, path) = (view.clone(), path.clone());
        async move {
            // Never more than the announced length, also when the file grew meanwhile.
            let wanted = size.saturating_sub(offset).min(DOWNLOAD_CHUNK_BYTES);
            if wanted == 0 {
                return None;
            }
            match view.read(&path, offset, wanted).await {
                // The file shrank or went away: end the body short, which the client sees as a broken transfer.
                Ok(chunk) if chunk.is_empty() => Some((Err(std::io::Error::other("the file changed")), size)),
                Ok(chunk) => {
                    let next = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                    Some((Ok(chunk), next))
                }
                Err(_) => Some((Err(std::io::Error::other("the file could not be read")), size)),
            }
        }
    })
}

/// How a file is handed to a client.
#[derive(Debug, PartialEq, Eq)]
struct Delivery {
    content_type: &'static str,
    inline: bool,
}

impl Delivery {
    /// The safe way to send content of type `media`, given what the client asked for.
    ///
    /// Only raster images and plain text are ever shown inline, and text
    /// always as `text/plain`. HTML, SVG and programs lose their type, so
    /// nothing renders or runs them even if a client ignores the disposition.
    fn of(media: MediaType, inline_requested: bool) -> Self {
        match media.class {
            MediaClass::Image => Self { content_type: media.mime, inline: inline_requested },
            MediaClass::Text if inline_requested => Self { content_type: "text/plain; charset=utf-8", inline: true },
            MediaClass::Text => Self { content_type: text_content_type(media.mime), inline: false },
            MediaClass::ActiveText | MediaClass::Executable => {
                Self { content_type: "application/octet-stream", inline: false }
            }
            _ => Self { content_type: media.mime, inline: false },
        }
    }
}

/// A text type with its charset (workspace text is UTF-8).
fn text_content_type(mime: &'static str) -> &'static str {
    match mime {
        "text/markdown" => "text/markdown; charset=utf-8",
        "text/csv" => "text/csv; charset=utf-8",
        "text/tab-separated-values" => "text/tab-separated-values; charset=utf-8",
        "text/plain" => "text/plain; charset=utf-8",
        other => other,
    }
}

/// `Content-Disposition` with the file name in both forms of RFC 6266: an
/// ASCII fallback in quotes and the exact name percent-encoded as UTF-8.
/// Nothing of the name is copied into the header unescaped.
fn content_disposition(inline: bool, file_name: &str) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let fallback: String = file_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') { c } else { '_' })
        .collect();
    let mut encoded = String::with_capacity(file_name.len());
    for byte in file_name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{kind}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(bytes: &[u8], extension: &str) -> MediaType {
        graphrt_files::sniff(bytes, Some(extension))
    }

    #[test]
    fn active_content_is_never_delivered_inline_or_with_its_type() {
        let page = media(b"<!doctype html><script>alert(1)</script>", "html");
        let drawing = media(b"<svg xmlns='http://www.w3.org/2000/svg' onload='x()'/>", "png");
        let program = media(b"\x7fELF\x02\x01\x01\0", "txt");
        for active in [page, drawing, program] {
            for asked in [true, false] {
                let delivery = Delivery::of(active, asked);
                assert_eq!(delivery, Delivery { content_type: "application/octet-stream", inline: false });
            }
        }
    }

    #[test]
    fn only_images_and_plain_text_are_shown_inline() {
        let png = media(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "png");
        assert_eq!(Delivery::of(png, true), Delivery { content_type: "image/png", inline: true });
        assert_eq!(Delivery::of(png, false), Delivery { content_type: "image/png", inline: false });
        // Text is shown as plain text whatever kind it is, so a browser treats none of it as a document.
        for (bytes, extension) in [(&b"a,b\n1,2\n"[..], "csv"), (b"<a><b/></a>", "xml"), (b"{\"a\":1}", "json")] {
            let delivery = Delivery::of(media(bytes, extension), true);
            assert_eq!(delivery, Delivery { content_type: "text/plain; charset=utf-8", inline: true });
        }
        assert_eq!(Delivery::of(media(b"a,b\n", "csv"), false).content_type, "text/csv; charset=utf-8");
        // A PDF can carry scripts and forms: it is a download.
        let pdf = Delivery::of(media(b"%PDF-1.7\n", "pdf"), true);
        assert_eq!(pdf, Delivery { content_type: "application/pdf", inline: false });
    }

    #[test]
    fn a_file_name_cannot_break_out_of_the_disposition_header() {
        let header = content_disposition(false, "a\"; filename=evil.html; x=\".txt");
        assert_eq!(
            header,
            "attachment; filename=\"a__ filename_evil.html_ x__.txt\"; \
             filename*=UTF-8''a%22%3B%20filename%3Devil.html%3B%20x%3D%22.txt"
        );
        let header = content_disposition(true, "laporan café.csv");
        assert_eq!(header, "inline; filename=\"laporan caf_.csv\"; filename*=UTF-8''laporan%20caf%C3%A9.csv");
        assert!(HeaderValue::from_str(&content_disposition(false, "x\r\nSet-Cookie: a=b")).is_ok());
        assert!(!content_disposition(false, "x\r\nSet-Cookie: a=b").contains('\n'));
    }

    #[tokio::test]
    async fn an_upload_stops_at_the_limit_without_buffering_the_rest() {
        let pieces = (0..1000).map(|_| Ok::<_, std::io::Error>(vec![0u8; 1024]));
        let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&polled);
        let stream = futures::stream::iter(pieces).inspect(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let result = read_upload(Body::from_stream(stream), 10 * 1024, Duration::from_secs(5)).await;
        assert_eq!(result, Err(UploadError::TooLarge { limit: 10 * 1024 }));
        // Eleven pieces were taken from the client, not a thousand.
        assert_eq!(polled.load(std::sync::atomic::Ordering::Relaxed), 11);
        let exact = Body::from(vec![7u8; 10 * 1024]);
        assert_eq!(read_upload(exact, 10 * 1024, Duration::from_secs(5)).await.map(|bytes| bytes.len()), Ok(10 * 1024));
    }

    #[tokio::test]
    async fn an_upload_that_stalls_is_dropped() {
        let first = futures::stream::iter([Ok::<_, std::io::Error>(vec![1u8; 16])]);
        let stalled = first.chain(futures::stream::pending());
        let result = read_upload(Body::from_stream(stalled), 1024, Duration::from_millis(30)).await;
        assert_eq!(result, Err(UploadError::Stalled));
    }
}
