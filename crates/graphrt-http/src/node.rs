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
    #[serde(default)]
    parse_json: ParseJson,
    /// Send `Idempotency-Key` on POST, PUT, PATCH and DELETE (default on).
    #[serde(default = "yes")]
    idempotency_key: bool,
}

/// How a JSON response body that fails to parse is handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ParseJson {
    /// Keep `body`, output `json: null` and record `json_error` in metadata.
    #[default]
    Lenient,
    /// Fail the node with `invalid_response`.
    Strict,
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
/// JSON content types, `json` (the parsed body, or `null` when the body is not valid
/// JSON — the parse error is reported in metadata `json_error`; set
/// `parse_json: strict` to fail instead).
///
/// Text bodies honour the `charset` content-type parameter for UTF-8, ISO-8859-1 and
/// windows-1252; other undecodable text is decoded lossily and flagged with
/// metadata `body_lossy: true`.
///
/// Redirects are followed by the node itself (up to the policy's `max_redirects`,
/// each hop re-validated). When a hop changes origin (scheme, host or port), every
/// credential header is dropped: `authorization`, `cookie`, `proxy-authorization`,
/// the configured custom auth header and any header whose value came from a secret.
///
/// Node-level `timeout` and `max_response_bytes` can only lower the limits of the
/// host's [`EgressPolicy`], never raise them.
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
    parse_json: ParseJson,
    idempotency_key: bool,
    default_client: OnceLock<Result<HttpClient, String>>,
}

/// Headers that always carry credentials.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// A rendered request, ready to send (possibly several times across redirects).
struct PreparedRequest {
    method: Method,
    url: reqwest::Url,
    headers: HeaderMap,
    /// Lower-case names of headers to drop when a redirect changes origin.
    sensitive: Vec<String>,
    body: Option<Vec<u8>>,
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
            parse_json: cfg.parse_json,
            idempotency_key: cfg.idempotency_key,
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
        let policy = client.policy();

        // URL and query parameters.
        let raw_url = self.url.render_exposed(pool);
        let mut url = reqwest::Url::parse(raw_url.trim())
            .map_err(|e| NodeError::fatal("invalid_input", format!("invalid URL `{}`: {e}", self.url.render(pool))))?;
        if !self.params.is_empty() {
            let mut query = url.query_pairs_mut();
            for (k, v) in &self.params {
                if let Some(key) = render_key(k, pool, "query parameter")? {
                    query.append_pair(&key, &v.render_exposed(pool));
                }
            }
        }
        client.check_url(&url).map_err(egress_error)?;

        // Headers: user headers first, then authorization (which wins).
        let mut headers = HeaderMap::new();
        let mut sensitive: Vec<String> = CREDENTIAL_HEADERS.iter().map(|h| (*h).to_owned()).collect();
        let mut shown_headers = IndexMap::new();
        for (k, v) in &self.headers {
            let Some(name) = render_key(k, pool, "header")? else { continue };
            let name = name.trim().to_ascii_lowercase();
            let exposed = v.render_exposed(pool);
            let shown = v.render(pool);
            if exposed != shown {
                // The value contains a secret: never forward it to another origin.
                sensitive.push(name.clone());
            }
            insert_header(&mut headers, &name, &exposed)?;
            shown_headers.insert(name, Value::from(shown));
        }
        // Side effects are at-least-once per node (retries, re-execution after a
        // preempted run resumes): the key is the same each time, so the server can
        // apply the request once. A header set by the workflow wins.
        if self.idempotency_key && has_side_effects(&self.method) && !headers.contains_key(IDEMPOTENCY_KEY) {
            let key = ctx.idempotency_key();
            insert_header(&mut headers, IDEMPOTENCY_KEY, &key)?;
            shown_headers.insert(IDEMPOTENCY_KEY.to_owned(), Value::from(key));
        }
        if let Some(auth) = &self.auth {
            let key = auth.api_key.render_exposed(pool);
            let (name, value) = match auth.scheme {
                AuthScheme::Bearer => ("authorization".to_owned(), format!("Bearer {key}")),
                AuthScheme::Basic => {
                    let encoded = if key.contains(':') { BASE64.encode(key.as_bytes()) } else { key };
                    ("authorization".to_owned(), format!("Basic {encoded}"))
                }
                AuthScheme::Custom => (auth.header.clone().unwrap_or_default().trim().to_ascii_lowercase(), key),
            };
            insert_header(&mut headers, &name, &value)?;
            sensitive.push(name.clone());
            shown_headers.insert(name, Value::from(SECRET_MASK));
        }

        // Body.
        let body = self.build_body(ctx)?;
        let body_len = body.as_ref().map_or(0, |(_, b)| b.len());
        let body = match body {
            Some((content_type, bytes)) => {
                if !headers.contains_key(reqwest::header::CONTENT_TYPE) {
                    insert_header(&mut headers, "content-type", &content_type)?;
                }
                Some(bytes)
            }
            None => None,
        };

        let mut shown_url = self.url.render(pool);
        if url.query().is_some() && !self.params.is_empty() {
            shown_url = format!("{shown_url}?{}", masked_query(&self.params, pool));
        }

        // Limits from node config can only lower the operator's caps.
        let total_timeout = self.timeout.map_or(policy.total_timeout, |t| t.min(policy.total_timeout));
        let total_timeout = match ctx.remaining() {
            Some(remaining) => total_timeout.min(remaining),
            None => total_timeout,
        };
        let cap = self.max_response_bytes.map_or(policy.max_response_bytes, |c| c.min(policy.max_response_bytes));

        let prepared = PreparedRequest { method: self.method.clone(), url, headers, sensitive, body };
        let (response, final_url, redirects) = self.send_following_redirects(client, prepared, total_timeout).await?;

        let request_meta = Value::object([
            ("method", Value::from(self.method.as_str())),
            ("url", Value::from(shown_url)),
            ("headers", Value::Object(Arc::new(shown_headers.into_iter().collect()))),
            ("body_bytes", Value::Int(i64::try_from(body_len).unwrap_or(i64::MAX))),
            ("redirects", Value::Int(i64::try_from(redirects).unwrap_or(i64::MAX))),
        ]);
        let output = self.read_response(response, cap, &final_url).await?;
        Ok(output.meta("request", request_meta))
    }

    /// Send the request, following redirects manually so credentials never cross origins.
    /// Returns the final response, its URL and the number of redirects followed.
    async fn send_following_redirects(
        &self,
        client: &HttpClient,
        mut request: PreparedRequest,
        total_timeout: Duration,
    ) -> Result<(reqwest::Response, reqwest::Url, usize), NodeError> {
        let started = std::time::Instant::now();
        let max_redirects = client.policy().max_redirects;
        let http = client
            .manual_redirect_client(self.ssl_verify)
            .map_err(|e| NodeError::fatal("http_client", e.to_string()))?;
        let mut redirects = 0usize;
        loop {
            let remaining = total_timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                let mut err = NodeError::new("timeout", "HTTP request timed out while following redirects");
                err.retryable = self.may_retry();
                return Err(err);
            }
            let mut builder = http
                .request(request.method.clone(), request.url.clone())
                .headers(request.headers.clone())
                .timeout(remaining);
            if let Some(bytes) = &request.body {
                builder = builder.body(bytes.clone());
            }
            let response = builder.send().await.map_err(|e| self.transport_error(e))?;

            let status = response.status();
            let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok());
            let Some(location) = location.filter(|_| status.is_redirection() && max_redirects > 0) else {
                return Ok((response, request.url, redirects));
            };
            if redirects >= max_redirects {
                return Err(NodeError::fatal(
                    "too_many_redirects",
                    format!("stopped after {max_redirects} redirects (policy limit)"),
                ));
            }
            let next = request.url.join(location).map_err(|_| {
                NodeError::fatal("invalid_response", format!("HTTP {status} with an invalid Location header"))
            })?;
            client.check_url(&next).map_err(egress_error)?;

            if next.origin() != request.url.origin() {
                for name in &request.sensitive {
                    request.headers.remove(name.as_str());
                }
            }
            let code = status.as_u16();
            let becomes_get = code == 303 || (matches!(code, 301 | 302) && request.method == Method::POST);
            if becomes_get {
                if request.method != Method::HEAD {
                    request.method = Method::GET;
                }
                request.body = None;
                request.headers.remove(reqwest::header::CONTENT_TYPE);
                request.headers.remove(reqwest::header::CONTENT_LENGTH);
            }
            request.url = next;
            redirects += 1;
        }
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
                    if let Some(key) = render_key(&item.key, pool, "form field")? {
                        form.append_pair(&key, &item.value.render_exposed(pool));
                    }
                }
                Some(("application/x-www-form-urlencoded".to_owned(), form.finish().into_bytes()))
            }
            BodyKind::FormData => {
                let mut fields = Vec::new();
                for item in self.body.data.items() {
                    let Some(name) = render_key(&item.key, pool, "form field")? else { continue };
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
        // Error responses only need a snippet; reading stops at the cap instead of failing,
        // so a large 5xx page is still reported as a (retryable) status error.
        let error_status = self.fail_on_error_status && !status.is_success();
        if let Some(len) = response.content_length()
            && !error_status
            && usize::try_from(len).map_or(true, |l| l > cap)
        {
            return Err(too_large(cap));
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
                        if error_status {
                            break;
                        }
                        return Err(too_large(cap));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(self.transport_error(e)),
            }
        }

        if error_status {
            let snippet: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
            let message = format!("HTTP {status} from {}: {snippet}", url.host_str().unwrap_or_default());
            let retryable_status = status.is_server_error() || matches!(status.as_u16(), 408 | 429);
            let mut err = NodeError::new("http_status", message);
            err.retryable = retryable_status && self.may_retry();
            if matches!(status.as_u16(), 429 | 503) {
                err.retry_after_ms = retry_after_ms;
            }
            return Err(err);
        }

        let status_code = Value::Int(i64::from(status.as_u16()));
        let headers = Value::Object(Arc::new(header_map.into_iter().collect()));
        let mut output = NodeOutput::new().with("status_code", status_code).with("headers", headers);

        let decoded = if is_binary(&content_type, &disposition) { None } else { decode_text(&bytes, &content_type) };
        match decoded {
            Some(DecodedText { text, lossy }) => {
                if is_json_type(&content_type) {
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(json) => output = output.with("json", Value::from_json(json)),
                        Err(e) if self.parse_json == ParseJson::Strict => {
                            return Err(NodeError::fatal(
                                "invalid_response",
                                format!("response declared as JSON is not valid JSON: {e}"),
                            ));
                        }
                        Err(e) => {
                            output = output.with("json", Value::Null).meta("json_error", e.to_string());
                        }
                    }
                }
                if lossy {
                    output = output.meta("body_lossy", true);
                }
                // What a reader sees of a page; the body itself for anything that is not HTML.
                let readable = if crate::readable::is_html_type(&content_type) {
                    crate::readable::html_to_text(&text)
                } else {
                    text.clone()
                };
                output = output.with("body", text).with("text", readable).with("files", Value::array([]));
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
                output = output.with("body", "").with("text", "").with("files", Value::array([Value::from(file)]));
            }
        }
        Ok(output)
    }

    /// Map a transport error. The URL is removed from the message: it may contain secrets.
    fn transport_error(&self, error: reqwest::Error) -> NodeError {
        let error = error.without_url();
        if let Some(egress) = find_egress_error(&error) {
            return egress_error(egress.clone());
        }
        if error.is_redirect() {
            return NodeError::fatal("egress_blocked", error_chain(&error));
        }
        let mut err = if error.is_timeout() {
            NodeError::new("timeout", error_chain(&error))
        } else if error.is_connect() {
            // Nothing was sent: retrying is safe for every method.
            return NodeError::new("connection", error_chain(&error));
        } else {
            NodeError::new("http", error_chain(&error))
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
        if let Some(auth) = &self.auth {
            out.extend(auth.api_key.selectors().cloned());
        }
        match &self.body.data {
            BodyData::Text(text) => out.extend(Template::parse(text).selectors().cloned()),
            BodyData::Items(items) => {
                for item in items {
                    out.extend(item.key.selectors().cloned());
                    out.extend(item.value.selectors().cloned());
                    out.extend(item.file.iter().cloned());
                }
            }
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

/// Header carrying [`NodeContext::idempotency_key`].
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Methods that change server state (RFC 9110 "unsafe" methods).
fn has_side_effects(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE)
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
///
/// A `null` value means "no file" (optional input). A selector that does not resolve
/// at all, or array items that are not files, are errors rather than silently
/// sending no file part.
fn files_at(pool: &VariablePool, selector: Option<&Selector>) -> Result<Vec<File>, NodeError> {
    let selector = selector.ok_or_else(|| NodeError::fatal("invalid_input", "file field has no `file` selector"))?;
    match pool.get(selector) {
        Some(Value::File(f)) => Ok(vec![(*f).clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::File(f) => Ok((**f).clone()),
                other => Err(NodeError::fatal(
                    "invalid_input",
                    format!("`{selector}` contains a {}, expected only files", other.type_name()),
                )),
            })
            .collect(),
        Some(Value::Null) => Ok(Vec::new()),
        None => Err(NodeError::fatal("invalid_input", format!("file variable `{selector}` not found"))),
        Some(other) => {
            Err(NodeError::fatal("invalid_input", format!("`{selector}` is a {}, not a file", other.type_name())))
        }
    }
}

/// Query string for display: secret-derived keys and values are masked.
fn masked_query(params: &[(Template, Template)], pool: &VariablePool) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        let key = k.render(pool);
        if !key.is_empty() {
            q.append_pair(&key, &v.render(pool));
        }
    }
    q.finish()
}

/// Render the key of a header / parameter / form field.
///
/// Returns `Ok(None)` for rows whose key template is blank (empty rows left in a
/// form), and an error when a non-blank key template renders empty — that usually
/// means a misspelled or missing variable, which must not silently drop the field.
fn render_key(template: &Template, pool: &VariablePool, what: &str) -> Result<Option<String>, NodeError> {
    if template.source().trim().is_empty() {
        return Ok(None);
    }
    let key = template.render_exposed(pool);
    if key.trim().is_empty() {
        return Err(NodeError::fatal(
            "invalid_input",
            format!("{what} name `{}` rendered empty (missing variable?)", template.source()),
        ));
    }
    Ok(Some(key))
}

/// Decoded text body.
struct DecodedText {
    text: String,
    /// Some bytes could not be decoded and were replaced.
    lossy: bool,
}

/// Decode a response body as text.
///
/// Honours the `charset` parameter for UTF-8, ISO-8859-1 / latin-1 and windows-1252.
/// Bodies with a textual content type are always decoded (lossily when needed); bodies
/// with an unknown or missing type are text only when they are valid UTF-8, otherwise
/// `None` (treated as binary).
fn decode_text(bytes: &[u8], content_type: &str) -> Option<DecodedText> {
    let charset = charset_of(content_type);
    match charset.as_deref() {
        Some("iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1" | "us-ascii" | "ascii") => {
            return Some(DecodedText { text: bytes.iter().map(|&b| char::from(b)).collect(), lossy: false });
        }
        Some("windows-1252" | "cp1252" | "x-cp1252") => {
            return Some(DecodedText { text: bytes.iter().map(|&b| windows_1252_char(b)).collect(), lossy: false });
        }
        _ => {}
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(DecodedText { text: text.to_owned(), lossy: false }),
        Err(_) if is_textual(content_type) || charset.is_some() => {
            Some(DecodedText { text: String::from_utf8_lossy(bytes).into_owned(), lossy: true })
        }
        Err(_) => None,
    }
}

/// The lower-cased `charset` parameter of a content type.
fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// windows-1252 differs from latin-1 only in 0x80..=0x9F.
fn windows_1252_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{02C6}',
        '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}', '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}',
        '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}',
        '\u{203A}', '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x80..=0x9F => HIGH.get(usize::from(b - 0x80)).copied().unwrap_or('\u{FFFD}'),
        _ => char::from(b),
    }
}

/// Content types that are text by definition.
fn is_textual(content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or_default().trim();
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct == "application/x-www-form-urlencoded"
}

fn is_json_type(content_type: &str) -> bool {
    content_type.split(';').next().unwrap_or_default().trim().contains("json")
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
