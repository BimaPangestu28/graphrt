//! REST / JSON APIs: one request per page (page numbers, offsets, cursors,
//! a next URL in the body or a `Link` header), records picked with JSONPath,
//! the fields to index named with JSONPath too.

use std::collections::HashSet;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use reqwest::Method;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::error::{ConnectError, Result};
use crate::http::SourceHttp;
use crate::jsonpath::{JsonPath, value_text};
use crate::types::{Connector, ItemContent, Listing, RemoteItem};

/// Most pages one sync may request.
pub const MAX_PAGES_LIMIT: usize = 1_000;
/// Most records one sync may index.
pub const MAX_ITEMS_LIMIT: usize = 50_000;

/// How the API splits its results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Pagination {
    /// One request.
    #[default]
    None,
    /// `?page=1`, `?page=2`, ... until a page has no records.
    Page {
        #[serde(default = "page_param")]
        param: String,
        #[serde(default = "one")]
        start: u64,
        /// Page size parameter and value, when the API takes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_param: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },
    /// `?offset=0&limit=100`, `?offset=100&limit=100`, ... until a short page.
    Offset {
        #[serde(default = "offset_param")]
        param: String,
        #[serde(default = "limit_param")]
        limit_param: String,
        #[serde(default = "hundred")]
        limit: u64,
    },
    /// The response names the next cursor (`cursor_path`), sent as `param`.
    Cursor { param: String, cursor_path: String },
    /// The response holds the address of the next page (`next_path`).
    NextUrl { next_path: String },
    /// RFC 8288 `Link: <...>; rel="next"` header (GitHub style).
    LinkHeader,
}

fn page_param() -> String {
    "page".into()
}
fn offset_param() -> String {
    "offset".into()
}
fn limit_param() -> String {
    "limit".into()
}
fn one() -> u64 {
    1
}
fn hundred() -> u64 {
    100
}
fn default_max_pages() -> usize {
    50
}
fn get() -> String {
    "GET".into()
}

/// Config of an `api` source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub url: String,
    /// `GET` (default) or `POST`.
    #[serde(default = "get")]
    pub method: String,
    /// Extra headers without secrets (`Accept`, an API version). Credentials go in `auth`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, String>,
    /// Extra query parameters.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub query: IndexMap<String, String>,
    /// JSON body of `POST` requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Where the records are in a response (default `$`: the response is the
    /// list, or the one record).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_path: Option<String>,
    /// Field of a record holding its id (default `id`, then the record's position).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_path: Option<String>,
    /// Fields to index, in order (default: the whole record).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_paths: Vec<String>,
    /// Field that changes when the record changes (`updated_at`); without
    /// it the record's content decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_path: Option<String>,
    /// Field holding a link to the record for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_path: Option<String>,
    #[serde(default)]
    pub pagination: Pagination,
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
    /// Allow private and local network addresses. Off by default.
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Debug)]
struct Paths {
    items: JsonPath,
    id: Option<JsonPath>,
    title: Option<JsonPath>,
    texts: Vec<JsonPath>,
    updated: Option<JsonPath>,
    url: Option<JsonPath>,
    cursor: Option<JsonPath>,
    next: Option<JsonPath>,
}

fn optional(path: Option<&String>) -> Result<Option<JsonPath>> {
    path.map(|p| JsonPath::parse(p)).transpose()
}

/// The `api` connector.
#[derive(Debug)]
pub struct ApiConnector {
    config: ApiConfig,
    method: Method,
    headers: Vec<(HeaderName, HeaderValue)>,
    paths: Paths,
    http: SourceHttp,
}

impl ApiConnector {
    /// @throws `ConnectError::Config` for a bad address, method, header or JSONPath.
    pub fn new(config: ApiConfig, http: SourceHttp) -> Result<Self> {
        let url = Url::parse(config.url.trim())
            .map_err(|_| ConnectError::Config(format!("`{}` is not an address (https://...)", config.url)))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConnectError::Config("API addresses start with https:// or http://".into()));
        }
        let method = match config.method.to_ascii_uppercase().as_str() {
            "GET" => Method::GET,
            "POST" => Method::POST,
            other => return Err(ConnectError::Config(format!("method `{other}` is not supported (GET or POST)"))),
        };
        if config.max_pages == 0 || config.max_pages > MAX_PAGES_LIMIT {
            return Err(ConnectError::Config(format!("pages per sync must be between 1 and {MAX_PAGES_LIMIT}")));
        }
        let mut headers = Vec::new();
        for (name, value) in &config.headers {
            let lowered = name.to_ascii_lowercase();
            if matches!(lowered.as_str(), "authorization" | "cookie" | "proxy-authorization") {
                return Err(ConnectError::Config(format!(
                    "put the `{name}` credential in the sign-in settings (auth), not in plain headers"
                )));
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConnectError::Config(format!("`{name}` is not a valid header name")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| ConnectError::Config(format!("the value of header `{name}` is not valid")))?;
            headers.push((name, value));
        }
        let paths = Paths {
            items: JsonPath::parse(config.items_path.as_deref().unwrap_or("$"))?,
            id: optional(config.id_path.as_ref())?,
            title: optional(config.title_path.as_ref())?,
            texts: config.text_paths.iter().map(|p| JsonPath::parse(p)).collect::<Result<_>>()?,
            updated: optional(config.updated_path.as_ref())?,
            url: optional(config.url_path.as_ref())?,
            cursor: match &config.pagination {
                Pagination::Cursor { cursor_path, .. } => Some(JsonPath::parse(cursor_path)?),
                _ => None,
            },
            next: match &config.pagination {
                Pagination::NextUrl { next_path } => Some(JsonPath::parse(next_path)?),
                _ => None,
            },
        };
        Ok(Self { config, method, headers, paths, http })
    }

    fn page_url(&self, extra: &[(String, String)]) -> Result<String> {
        let mut url = Url::parse(self.config.url.trim())
            .map_err(|_| ConnectError::Config(format!("`{}` is not an address", self.config.url)))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in &self.config.query {
                pairs.append_pair(key, value);
            }
            for (key, value) in extra {
                pairs.append_pair(key, value);
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        Ok(url.into())
    }

    fn record(&self, record: &Value, position: usize) -> RemoteItem {
        let pick = |path: &Option<JsonPath>| path.as_ref().and_then(|p| p.first(record)).map(value_text);
        let id = pick(&self.paths.id)
            .or_else(|| record.get("id").map(value_text))
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("#{position}"));
        let title = pick(&self.paths.title).filter(|t| !t.is_empty()).unwrap_or_else(|| id.clone());
        let text = if self.paths.texts.is_empty() {
            serde_json::to_string_pretty(record).unwrap_or_default()
        } else {
            self.paths
                .texts
                .iter()
                .flat_map(|path| path.select(record))
                .map(value_text)
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        RemoteItem {
            id,
            title,
            url: pick(&self.paths.url).filter(|u| u.starts_with("http")),
            version: pick(&self.paths.updated).filter(|v| !v.is_empty()).map(|v| format!("updated:{v}")),
            mime_type: None,
            content: Some(ItemContent::Text(text)),
        }
    }
}

/// The `rel="next"` address of an RFC 8288 `Link` header.
pub fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        let is_next = params.split(';').any(|p| {
            let p = p.trim().to_ascii_lowercase();
            p == "rel=\"next\"" || p == "rel=next"
        });
        is_next.then(|| target.trim().trim_start_matches('<').trim_end_matches('>').to_owned())
    })
}

impl Connector for ApiConnector {
    fn kind(&self) -> &'static str {
        "api"
    }

    fn list(&self) -> BoxFuture<'_, Result<Listing>> {
        Box::pin(async move {
            let mut listing = Listing { complete: false, ..Listing::default() };
            let mut seen = HashSet::new();
            let mut next_url: Option<String> = Some(self.page_url(&self.first_page_params())?);
            let mut position = 0usize;
            // `page_index`: pages fetched so far, this one included.
            for page_index in (1u64..).take(self.config.max_pages) {
                let Some(url) = next_url.take() else {
                    listing.complete = true;
                    break;
                };
                let response = self
                    .http
                    .send(self.method.clone(), &url, |mut request| {
                        request = request.header(reqwest::header::ACCEPT, "application/json");
                        for (name, value) in &self.headers {
                            request = request.header(name.clone(), value.clone());
                        }
                        match &self.config.body {
                            Some(body) if self.method == Method::POST => request.json(body),
                            _ => request,
                        }
                    })
                    .await?;
                let link =
                    response.headers().get(reqwest::header::LINK).and_then(|v| v.to_str().ok()).and_then(next_link);
                let bytes = crate::http::read_limited(response, self.http.limits().max_json_bytes).await?;
                let json: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ConnectError::Decode(format!("the API returned something that is not JSON: {e}")))?;
                let selected = self.paths.items.select(&json);
                let records: Vec<&Value> = match selected.as_slice() {
                    [Value::Array(items)] => items.iter().collect(),
                    _ => selected,
                };
                let count = records.len();
                for record in records {
                    let item = self.record(record, position);
                    position += 1;
                    if seen.insert(item.id.clone()) {
                        listing.items.push(item);
                    }
                    if listing.items.len() >= MAX_ITEMS_LIMIT {
                        listing.warnings.push(format!("stopped at {MAX_ITEMS_LIMIT} records"));
                        return Ok(listing);
                    }
                }
                next_url = match &self.config.pagination {
                    Pagination::None => None,
                    Pagination::Page { param, start, size_param, size } => (count > 0)
                        .then(|| {
                            let mut params = vec![(param.clone(), (start + page_index).to_string())];
                            if let (Some(size_param), Some(size)) = (size_param, size) {
                                params.push((size_param.clone(), size.to_string()));
                            }
                            self.page_url(&params)
                        })
                        .transpose()?,
                    Pagination::Offset { param, limit_param, limit } => {
                        (u64::try_from(count).unwrap_or(u64::MAX) >= *limit && count > 0)
                            .then(|| {
                                self.page_url(&[
                                    (param.clone(), (page_index * limit).to_string()),
                                    (limit_param.clone(), limit.to_string()),
                                ])
                            })
                            .transpose()?
                    }
                    Pagination::Cursor { param, .. } => {
                        let cursor = self.paths.cursor.as_ref().and_then(|p| p.first(&json)).map(value_text);
                        cursor
                            .filter(|c| !c.is_empty() && c != "null")
                            .map(|c| self.page_url(&[(param.clone(), c)]))
                            .transpose()?
                    }
                    Pagination::NextUrl { .. } => self
                        .paths
                        .next
                        .as_ref()
                        .and_then(|p| p.first(&json))
                        .and_then(Value::as_str)
                        .filter(|n| !n.is_empty())
                        .map(|n| Url::parse(&url).and_then(|base| base.join(n)).map(String::from))
                        .transpose()
                        .map_err(|_| ConnectError::Decode("the next page address is not valid".into()))?,
                    Pagination::LinkHeader => link
                        .map(|n| Url::parse(&url).and_then(|base| base.join(&n)).map(String::from))
                        .transpose()
                        .map_err(|_| ConnectError::Decode("the next page address is not valid".into()))?,
                };
            }
            if next_url.is_none() {
                listing.complete = true;
            } else if !listing.complete {
                listing.warnings.push(format!(
                    "stopped after {} pages; raise the page limit to index every record",
                    self.config.max_pages
                ));
            }
            Ok(listing)
        })
    }

    fn fetch<'a>(&'a self, item: &'a RemoteItem) -> BoxFuture<'a, Result<ItemContent>> {
        Box::pin(async move {
            item.content
                .clone()
                .ok_or_else(|| ConnectError::NotFound(format!("record `{}` is no longer in the API", item.id)))
        })
    }
}

impl ApiConnector {
    fn first_page_params(&self) -> Vec<(String, String)> {
        match &self.config.pagination {
            Pagination::Page { param, start, size_param, size } => {
                let mut params = vec![(param.clone(), start.to_string())];
                if let (Some(size_param), Some(size)) = (size_param, size) {
                    params.push((size_param.clone(), size.to_string()));
                }
                params
            }
            Pagination::Offset { param, limit_param, limit } => {
                vec![(param.clone(), "0".into()), (limit_param.clone(), limit.to_string())]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_headers() {
        let header =
            r#"<https://api.example/items?page=2>; rel="next", <https://api.example/items?page=9>; rel="last""#;
        assert_eq!(next_link(header).as_deref(), Some("https://api.example/items?page=2"));
        assert_eq!(next_link(r#"<https://x/1>; rel="prev""#), None);
    }

    #[test]
    fn pagination_config_parses() {
        let config: ApiConfig = serde_json::from_value(serde_json::json!({
            "url": "https://api.example/items",
            "items_path": "$.data",
            "pagination": { "type": "cursor", "param": "cursor", "cursor_path": "$.meta.next" }
        }))
        .unwrap();
        assert_eq!(config.method, "GET");
        assert_eq!(config.max_pages, 50);
        assert!(matches!(config.pagination, Pagination::Cursor { .. }));
    }
}
