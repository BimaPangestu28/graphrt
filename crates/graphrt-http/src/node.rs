//! The `http-request` node.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, parse_config};
use graphrt_core::template::Template;
use graphrt_core::value::{File, FileKind, FileTransfer, SECRET_MASK};
use graphrt_core::{Selector, Value, VariablePool};

use crate::body::{MultipartField, data_url, encode_multipart, inline_file_bytes, render_json};
use crate::client::HttpClient;
use crate::policy::{EgressError, EgressPolicy};

/// Node kind handled by [`HttpRequestNode`].
pub const KIND: &str = "http-request";

/// Binary responses up to this size are returned inline as `data:` URL files.
pub const INLINE_FILE_LIMIT: usize = 1024 * 1024;

/// Key/value list accepted as `"k: v\nk2: v2"`, `[{key, value}]` or `{k: v}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum KvSpec {
    Text(String),
    List(Vec<KvItem>),
    Map(IndexMap<String, Template>),
}

impl Default for KvSpec {
    fn default() -> Self {
        KvSpec::List(Vec::new())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct KvItem {
    key: Template,
    #[serde(default)]
    value: Template,
}

impl KvSpec {
    /// Parsed pairs of templates (legacy text lines are split on the first `:`).
    fn pairs(&self) -> Vec<(Template, Template)> {
        match self {
            KvSpec::Text(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| match line.split_once(':') {
                    Some((k, v)) => (Template::parse(k.trim()), Template::parse(v.trim())),
                    None => (Template::parse(line.trim()), Template::parse("")),
                })
                .collect(),
            KvSpec::List(items) => items.iter().map(|i| (i.key.clone(), i.value.clone())).collect(),
            KvSpec::Map(map) => map.iter().map(|(k, v)| (Template::parse(k), v.clone())).collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Authorization {
    #[serde(rename = "type", default)]
    kind: AuthKind,
    #[serde(default)]
    config: Option<AuthConfig>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AuthKind {
    #[default]
    NoAuth,
    ApiKey,
}

#[derive(Clone, Debug, Deserialize)]
struct AuthConfig {
    #[serde(rename = "type", default)]
    scheme: AuthScheme,
    #[serde(default)]
    api_key: Template,
    #[serde(default)]
    header: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AuthScheme {
    #[default]
    Bearer,
    Basic,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum BodyKind {
    #[default]
    None,
    Json,
    RawText,
    FormData,
    XWwwFormUrlencoded,
    Binary,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct BodyConfig {
    #[serde(rename = "type", default)]
    kind: BodyKind,
    #[serde(default)]
    data: BodyData,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum BodyData {
    Text(String),
    Items(Vec<BodyItem>),
}

impl Default for BodyData {
    fn default() -> Self {
        BodyData::Items(Vec::new())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct BodyItem {
    #[serde(default)]
    key: Template,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    value: Template,
    #[serde(default)]
    file: Option<Selector>,
}

impl BodyData {
    /// Raw template for json / raw-text bodies.
    fn raw(&self) -> Template {
        match self {
            BodyData::Text(t) => Template::parse(t),
            BodyData::Items(items) => items.first().map(|i| i.value.clone()).unwrap_or_default(),
        }
    }

    fn items(&self) -> &[BodyItem] {
        match self {
            BodyData::Text(_) => &[],
            BodyData::Items(items) => items,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct TimeoutConfig {
    #[serde(default, alias = "max_connect_timeout")]
    connect: Option<f64>,
    #[serde(default, alias = "max_read_timeout")]
    read: Option<f64>,
    #[serde(default, alias = "max_write_timeout")]
    write: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct HttpConfig {
    #[serde(default = "default_method")]
    method: String,
    url: Template,
    #[serde(default)]
    headers: KvSpec,
    #[serde(default)]
    params: KvSpec,
    #[serde(default)]
    authorization: Authorization,
    #[serde(default)]
    body: Option<BodyConfig>,
    #[serde(default)]
    timeout: Option<TimeoutConfig>,
    #[serde(default = "yes")]
    ssl_verify: bool,
    #[serde(default = "yes")]
    fail_on_error_status: bool,
    #[serde(default)]
    max_response_bytes: Option<usize>,
    #[serde(default)]
    retry_non_idempotent: bool,
}

fn default_method() -> String {
    "get".to_owned()
}

fn yes() -> bool {
    true
}

/// Makes an HTTP request.
///
/// Outputs: `status_code`, `body` (text; empty for binary responses), `headers`
/// (object, lower-case names), `files` (binary responses as inline files) and, for
/// JSON responses, `json` (the parsed body).
#[derive(Debug)]
pub struct HttpRequestNode {
    method: Method,
    url: Template,
    headers: Vec<(Template, Template)>,
    params: Vec<(Template, Template)>,
    auth: Option<AuthConfig>,
    body: BodyConfig,
    timeout: Option<Duration>,
    ssl_verify: bool,
    fail_on_error_status: bool,
    max_response_bytes: Option<usize>,
    retry_non_idempotent: bool,
    default_client: OnceLock<Result<HttpClient, String>>,
}

impl HttpRequestNode {
    /// Build from a node document.
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let cfg: HttpConfig = parse_config(doc)?;
        let method = parse_method(&cfg.method)
            .ok_or_else(|| ConfigError::new(doc, format!("unsupported HTTP method `{}`", cfg.method)))?;
        if cfg.url.source().trim().is_empty() {
            return Err(ConfigError::new(doc, "`url` must not be empty"));
        }
        let auth = match cfg.authorization.kind {
            AuthKind::NoAuth => None,
            AuthKind::ApiKey => {
                let config = cfg
                    .authorization
                    .config
                    .ok_or_else(|| ConfigError::new(doc, "api-key authorization requires `config`"))?;
                if config.scheme == AuthScheme::Custom && config.header.as_deref().is_none_or(|h| h.trim().is_empty()) {
                    return Err(ConfigError::new(doc, "custom authorization requires a `header` name"));
                }
                Some(config)
            }
        };
        let timeout = cfg.timeout.and_then(|t| {
            let total: f64 = [t.connect, t.read, t.write].iter().flatten().filter(|s| s.is_finite() && **s > 0.0).sum();
            (total > 0.0).then(|| Duration::from_secs_f64(total.min(86_400.0)))
        });
        Ok(Self {
            method,
            url: cfg.url,
            headers: cfg.headers.pairs(),
            params: cfg.params.pairs(),
            auth,
            body: cfg.body.unwrap_or_default(),
            timeout,
            ssl_verify: cfg.ssl_verify,
            fail_on_error_status: cfg.fail_on_error_status,
            max_response_bytes: cfg.max_response_bytes,
            retry_non_idempotent: cfg.retry_non_idempotent,
            default_client: OnceLock::new(),
        })
    }

    fn client<'a>(&'a self, ctx: &'a NodeContext) -> Result<&'a HttpClient, NodeError> {
        if let Some(client) = ctx.service::<HttpClient>() {
            return Ok(client);
        }
        self.default_client
            .get_or_init(|| HttpClient::new(EgressPolicy::default()).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| NodeError::fatal("http_client", e.clone()))
    }

    fn may_retry(&self) -> bool {
        self.retry_non_idempotent || is_idempotent(&self.method)
    }

    async fn execute(&self, ctx: &NodeContext) -> Result<NodeOutput, NodeError> {
        let pool = &ctx.pool;
        let client = self.client(ctx)?;

        // URL and query parameters.
        let raw_url = self.url.render_exposed(pool);
        let mut url = reqwest::Url::parse(raw_url.trim())
            .map_err(|e| NodeError::fatal("invalid_input", format!("invalid URL `{}`: {e}", self.url.render(pool))))?;
        if !self.params.is_empty() {
            let mut query = url.query_pairs_mut();
            for (k, v) in &self.params {
                let key = k.render_exposed(pool);
                if !key.is_empty() {
                    query.append_pair(&key, &v.render_exposed(pool));
                }
            }
        }
        client.check_url(&url).map_err(egress_error)?;

        // Headers: user headers first, then authorization (which wins).
        let mut headers = HeaderMap::new();
        let mut shown_headers = IndexMap::new();
        for (k, v) in &self.headers {
            let name = k.render_exposed(pool);
            if name.trim().is_empty() {
                continue;
            }
            insert_header(&mut headers, name.trim(), &v.render_exposed(pool))?;
            shown_headers.insert(name.trim().to_ascii_lowercase(), Value::from(v.render(pool)));
        }
        if let Some(auth) = &self.auth {
            let key = auth.api_key.render_exposed(pool);
            let (name, value) = match auth.scheme {
                AuthScheme::Bearer => ("authorization".to_owned(), format!("Bearer {key}")),
                AuthScheme::Basic => {
                    let encoded = if key.contains(':') { BASE64.encode(key.as_bytes()) } else { key };
                    ("authorization".to_owned(), format!("Basic {encoded}"))
                }
                AuthScheme::Custom => (auth.header.clone().unwrap_or_default(), key),
            };
            insert_header(&mut headers, name.trim(), &value)?;
            shown_headers.insert(name.trim().to_ascii_lowercase(), Value::from(SECRET_MASK));
        }

        // Body.
        let body = self.build_body(ctx)?;
        if let Some((content_type, _)) = &body {
            if !headers.contains_key(reqwest::header::CONTENT_TYPE) {
                insert_header(&mut headers, "content-type", content_type)?;
            }
        }

        let mut request = client.client(self.ssl_verify).request(self.method.clone(), url.clone()).headers(headers);
        let timeout = self.timeout.unwrap_or(client.policy().total_timeout);
        request = request.timeout(timeout);
        let body_len = body.as_ref().map_or(0, |(_, b)| b.len());
        if let Some((_, bytes)) = body {
            request = request.body(bytes);
        }

        let mut shown_url = self.url.render(pool);
        if let Some(query) = url.query() {
            if !self.params.is_empty() {
                shown_url = format!("{shown_url}?{}", masked_query(&self.params, pool, query));
            }
        }
        let request_meta = Value::object([
            ("method", Value::from(self.method.as_str())),
            ("url", Value::from(shown_url)),
            ("headers", Value::Object(Arc::new(shown_headers.into_iter().collect()))),
            ("body_bytes", Value::Int(i64::try_from(body_len).unwrap_or(i64::MAX))),
        ]);

        let response = request.send().await.map_err(|e| self.transport_error(&e))?;
        let cap = self.max_response_bytes.unwrap_or(client.policy().max_response_bytes);
        let output = self.read_response(response, cap, &url).await?;
        Ok(output.meta("request", request_meta))
    }

    fn build_body(&self, ctx: &NodeContext) -> Result<Option<(String, Vec<u8>)>, NodeError> {
        let pool = &ctx.pool;
        Ok(match self.body.kind {
            BodyKind::None => None,
            BodyKind::Json => {
                let value = render_json(&self.body.data.raw(), pool)?;
                if value.is_null() && self.body.data.raw().source().trim().is_empty() {
                    None
                } else {
                    let bytes = serde_json::to_vec(&value)
                        .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode JSON body: {e}")))?;
                    Some(("application/json".to_owned(), bytes))
                }
            }
            BodyKind::RawText => {
                Some(("text/plain; charset=utf-8".to_owned(), self.body.data.raw().render_exposed(pool).into_bytes()))
            }
            BodyKind::XWwwFormUrlencoded => {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                for item in self.body.data.items() {
                    let key = item.key.render_exposed(pool);
                    if !key.is_empty() {
                        form.append_pair(&key, &item.value.render_exposed(pool));
                    }
                }
                Some(("application/x-www-form-urlencoded".to_owned(), form.finish().into_bytes()))
            }
            BodyKind::FormData => {
                let mut fields = Vec::new();
                for item in self.body.data.items() {
                    let name = item.key.render_exposed(pool);
                    if name.is_empty() {
                        continue;
                    }
                    if item.kind.as_deref() == Some("file") {
                        for file in files_at(pool, item.file.as_ref())? {
                            fields.push(MultipartField::File {
                                name: name.clone(),
                                filename: file.name.clone(),
                                mime: file.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
                                bytes: inline_file_bytes(&file)?,
                            });
                        }
                    } else {
                        fields.push(MultipartField::Text { name, value: item.value.render_exposed(pool) });
                    }
                }
                let boundary = format!("graphrt-{}", ctx.exec_id.replace(|c: char| !c.is_ascii_alphanumeric(), ""));
                let (content_type, body) = encode_multipart(&fields, &boundary);
                Some((content_type, body))
            }
            BodyKind::Binary => {
                let selector = self.body.data.items().first().and_then(|i| i.file.as_ref());
                let file = files_at(pool, selector)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| NodeError::fatal("invalid_input", "binary body requires a file"))?;
                let mime = file.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned());
                Some((mime, inline_file_bytes(&file)?))
            }
        })
    }

    async fn read_response(
        &self,
        mut response: reqwest::Response,
        cap: usize,
        url: &reqwest::Url,
    ) -> Result<NodeOutput, NodeError> {
        let status = response.status();
        if let Some(len) = response.content_length() {
            if usize::try_from(len).map_or(true, |l| l > cap) {
                return Err(too_large(cap));
            }
        }
        let mut header_map: IndexMap<String, Value> = IndexMap::new();
        for (name, value) in response.headers() {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            let key = name.as_str().to_owned();
            let merged = match header_map.get(&key).and_then(Value::as_str) {
                Some(prev) => format!("{prev}, {text}"),
                None => text,
            };
            header_map.insert(key, Value::from(merged));
        }
        let content_type =
            header_map.get("content-type").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
        let disposition = header_map.get("content-disposition").and_then(Value::as_str).unwrap_or_default().to_owned();
        let retry_after_ms = header_map.get("retry-after").and_then(Value::as_str).and_then(parse_retry_after_ms);

        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len().saturating_add(chunk.len()) > cap {
                        return Err(too_large(cap));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(self.transport_error(&e)),
            }
        }

        let status_code = Value::Int(i64::from(status.as_u16()));
        let headers = Value::Object(Arc::new(header_map.into_iter().collect()));
        let mut output = NodeOutput::new().with("status_code", status_code).with("headers", headers);

        let text = if is_binary(&content_type, &disposition) { None } else { std::str::from_utf8(&bytes).ok() };
        match text {
            Some(text) => {
                if content_type.contains("json") {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
                        output = output.with("json", Value::from_json(json));
                    }
                }
                output = output.with("body", text.to_owned()).with("files", Value::array([]));
            }
            None => {
                if bytes.len() > INLINE_FILE_LIMIT {
                    return Err(NodeError::fatal(
                        "response_too_large",
                        format!(
                            "binary response of {} bytes exceeds the inline file limit of {INLINE_FILE_LIMIT} bytes",
                            bytes.len()
                        ),
                    ));
                }
                let mime = content_type.split(';').next().unwrap_or_default().trim();
                let mime = if mime.is_empty() { "application/octet-stream" } else { mime };
                let name = filename_from_disposition(&disposition)
                    .or_else(|| {
                        url.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).map(str::to_owned)
                    })
                    .unwrap_or_else(|| "download".to_owned());
                let file = File {
                    marker: true,
                    id: None,
                    extension: name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()),
                    name,
                    kind: file_kind(mime),
                    transfer: FileTransfer::Inline,
                    url: Some(data_url(mime, &bytes)),
                    reference: None,
                    mime_type: Some(mime.to_owned()),
                    size: u64::try_from(bytes.len()).ok(),
                };
                output = output.with("body", "").with("files", Value::array([Value::from(file)]));
            }
        }

        if self.fail_on_error_status && !status.is_success() {
            let snippet: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
            let message = format!("HTTP {status} from {}: {snippet}", url.host_str().unwrap_or_default());
            let retryable_status = status.is_server_error() || status.as_u16() == 429 || status.as_u16() == 408;
            let mut err = NodeError::new("http_status", message);
            err.retryable = retryable_status && self.may_retry();
            if matches!(status.as_u16(), 429 | 503) {
                err.retry_after_ms = retry_after_ms;
            }
            return Err(err);
        }
        Ok(output)
    }

    fn transport_error(&self, error: &reqwest::Error) -> NodeError {
        if let Some(egress) = find_egress_error(error) {
            return egress_error(egress.clone());
        }
        if error.is_redirect() {
            return NodeError::fatal("egress_blocked", error_chain(error));
        }
        let mut err = if error.is_timeout() {
            NodeError::new("timeout", error_chain(error))
        } else if error.is_connect() {
            // Nothing was sent: retrying is safe for every method.
            return NodeError::new("connection", error_chain(error));
        } else {
            NodeError::new("http", error_chain(error))
        };
        err.retryable = self.may_retry();
        err
    }
}

impl Node for HttpRequestNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { Ok(self.execute(&ctx).await?.into()) })
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out: Vec<Selector> = self.url.selectors().cloned().collect();
        for (k, v) in self.headers.iter().chain(self.params.iter()) {
            out.extend(k.selectors().cloned());
            out.extend(v.selectors().cloned());
        }
        out
    }
}

fn parse_method(method: &str) -> Option<Method> {
    Some(match method.trim().to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        _ => return None,
    })
}

fn is_idempotent(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), NodeError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| NodeError::fatal("invalid_input", format!("invalid header name `{name}`")))?;
    let value = HeaderValue::from_str(value.trim())
        .map_err(|_| NodeError::fatal("invalid_input", format!("invalid value for header `{name}`")))?;
    headers.insert(name, value);
    Ok(())
}

/// Files referenced by a selector (a single file or an array of files).
fn files_at(pool: &VariablePool, selector: Option<&Selector>) -> Result<Vec<File>, NodeError> {
    let selector = selector.ok_or_else(|| NodeError::fatal("invalid_input", "file field has no `file` selector"))?;
    match pool.get(selector) {
        Some(Value::File(f)) => Ok(vec![(*f).clone()]),
        Some(Value::Array(items)) => Ok(items.iter().filter_map(|v| v.as_file().cloned()).collect()),
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(other) => {
            Err(NodeError::fatal("invalid_input", format!("`{selector}` is a {}, not a file", other.type_name())))
        }
    }
}

/// Query string for display, with secret-derived values masked.
fn masked_query(params: &[(Template, Template)], pool: &VariablePool, _actual: &str) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        let key = k.render(pool);
        if !key.is_empty() {
            q.append_pair(&key, &v.render(pool));
        }
    }
    q.finish()
}

fn is_binary(content_type: &str, disposition: &str) -> bool {
    if disposition.to_ascii_lowercase().contains("attachment") {
        return true;
    }
    let ct = content_type.split(';').next().unwrap_or_default().trim();
    if ct.is_empty()
        || ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
    {
        return false;
    }
    ct.starts_with("image/")
        || ct.starts_with("audio/")
        || ct.starts_with("video/")
        || ct.starts_with("font/")
        || matches!(
            ct,
            "application/octet-stream"
                | "application/pdf"
                | "application/zip"
                | "application/gzip"
                | "application/x-tar"
                | "application/msword"
        )
        || ct.starts_with("application/vnd.")
}

fn file_kind(mime: &str) -> FileKind {
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime == "application/pdf" || mime.starts_with("application/vnd.") || mime == "application/msword" {
        FileKind::Document
    } else {
        FileKind::Custom
    }
}

fn filename_from_disposition(disposition: &str) -> Option<String> {
    disposition.split(';').find_map(|part| {
        let part = part.trim();
        let value = part.strip_prefix("filename=")?;
        let name = value.trim_matches('"').rsplit(['/', '\\']).next().unwrap_or_default().trim();
        (!name.is_empty()).then(|| name.to_owned())
    })
}

/// Parse a `Retry-After` header given in delay-seconds (the HTTP-date form is
/// ignored: the node's own retry policy then applies). Capped at one hour.
fn parse_retry_after_ms(value: &str) -> Option<u64> {
    let seconds: u64 = value.trim().parse().ok()?;
    Some(seconds.min(3600).saturating_mul(1000))
}

fn too_large(cap: usize) -> NodeError {
    NodeError::fatal("response_too_large", format!("response body exceeds the limit of {cap} bytes"))
}

fn egress_error(e: EgressError) -> NodeError {
    NodeError::fatal("egress_blocked", e.to_string())
}

fn find_egress_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a EgressError> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if let Some(egress) = e.downcast_ref::<EgressError>() {
            return Some(egress);
        }
        current = e.source();
    }
    None
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut current = error.source();
    while let Some(e) = current {
        let text = e.to_string();
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        current = e.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn retry_after_parsing() {
        assert_eq!(super::parse_retry_after_ms(" 2 "), Some(2000));
        assert_eq!(super::parse_retry_after_ms("999999"), Some(3_600_000));
        assert_eq!(super::parse_retry_after_ms("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    use super::*;

    fn doc(config: serde_json::Value) -> NodeDoc {
        NodeDoc::new("h", KIND, config)
    }

    #[test]
    fn kv_text_form() {
        let spec = KvSpec::Text("a: 1\n\nb:two:three\nflag".into());
        let pool = VariablePool::new();
        let pairs: Vec<(String, String)> =
            spec.pairs().iter().map(|(k, v)| (k.render(&pool), v.render(&pool))).collect();
        assert_eq!(pairs, vec![("a".into(), "1".into()), ("b".into(), "two:three".into()), ("flag".into(), "".into())]);
    }

    #[test]
    fn kv_list_and_map_forms() {
        let list: KvSpec = serde_json::from_value(serde_json::json!([{"key": "x", "value": "{{#n.v#}}"}])).unwrap();
        let map: KvSpec = serde_json::from_value(serde_json::json!({"x": "{{#n.v#}}"})).unwrap();
        let mut pool = VariablePool::new();
        pool.insert("n", "v", Value::from("val"));
        for spec in [list, map] {
            let pairs = spec.pairs();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].1.render(&pool), "val");
        }
    }

    #[test]
    fn config_validation() {
        assert!(HttpRequestNode::from_doc(&doc(serde_json::json!({"url": "https://x", "method": "TRACE"}))).is_err());
        assert!(HttpRequestNode::from_doc(&doc(serde_json::json!({"url": " "}))).is_err());
        assert!(
            HttpRequestNode::from_doc(&doc(serde_json::json!({
                "url": "https://x",
                "authorization": {"type": "api-key", "config": {"type": "custom", "api_key": "k"}}
            })))
            .is_err()
        );
        let n = HttpRequestNode::from_doc(&doc(serde_json::json!({
            "url": "https://x", "method": "options", "timeout": {"connect": 1, "read": 2.5, "write": 0}
        })))
        .unwrap();
        assert_eq!(n.method, Method::OPTIONS);
        assert_eq!(n.timeout, Some(Duration::from_millis(3500)));
    }

    #[test]
    fn binary_classification() {
        assert!(is_binary("image/png", ""));
        assert!(is_binary("application/pdf", ""));
        assert!(is_binary("text/plain", "attachment; filename=a.txt"));
        assert!(!is_binary("application/json; charset=utf-8", ""));
        assert!(!is_binary("text/html", ""));
        assert!(!is_binary("", ""));
        assert_eq!(filename_from_disposition("attachment; filename=\"../r.pdf\""), Some("r.pdf".into()));
    }
}
