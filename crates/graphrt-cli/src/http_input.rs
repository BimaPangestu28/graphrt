//! Request bodies and query strings of the HTTP API, without axum's `Json<T>`
//! and `Query<T>` extractors.
//!
//! Those extractors wrap every request type in `serde_path_to_error`, which is
//! compiled again for each type. [`JsonBody`] answers like axum's `Json` (415
//! without a JSON content type, 400 for invalid JSON, 422 for a body of the
//! wrong shape, with the same texts) but finds the offending path only when
//! there is an error ([`graphrt_core::value_path`]). Query strings are parsed
//! into [`QueryParams`] once and read field by field.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

/// A JSON request body deserialized into `T` (drop-in for `axum::Json` as an extractor).
#[derive(Debug)]
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Response> {
        let json = is_json_content_type(request.headers());
        let bytes = Bytes::from_request(request, state).await.map_err(IntoResponse::into_response)?;
        if !json {
            return Err(missing_json_content_type());
        }
        serde_json::from_slice(&bytes).map(JsonBody).map_err(|error| json_rejection(&bytes, &error))
    }
}

/// `application/json` or `application/*+json`, parameters ignored.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let Some(subtype) = essence.strip_prefix("application/") else { return false };
    subtype == "json" || subtype.ends_with("+json")
}

fn text(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

fn missing_json_content_type() -> Response {
    text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Expected request with `Content-Type: application/json`".to_owned())
}

/// axum's texts: 400 `Failed to parse the request body as JSON: …` for a syntax
/// error, 422 `Failed to deserialize the JSON body into the target type: path: …`.
#[inline(never)]
fn json_rejection(body: &[u8], error: &serde_json::Error) -> Response {
    use serde_json::error::Category;
    match error.classify() {
        Category::Data => {
            let path = serde_json::from_slice::<serde_json::Value>(body)
                .map(|value| graphrt_core::value_path::error_path(&value, error))
                .unwrap_or_default();
            let detail = if path.is_root() { error.to_string() } else { format!("{path}: {error}") };
            text(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Failed to deserialize the JSON body into the target type: {detail}"),
            )
        }
        Category::Syntax | Category::Eof | Category::Io => {
            text(StatusCode::BAD_REQUEST, format!("Failed to parse the request body as JSON: {error}"))
        }
    }
}

/// Decoded `key=value` pairs of a query string (form encoding: `+` is a space).
#[derive(Debug, Default)]
pub struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    /// Parses a raw query string (`None`: no query).
    pub fn parse(raw: Option<&str>) -> Self {
        let pairs = raw
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key), decode(value))
            })
            .collect();
        Self(pairs)
    }

    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }

    /// `key` as an owned string.
    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).map(str::to_owned)
    }

    /// `key` as an unsigned integer; `400` naming the field when it is not one.
    ///
    /// # Errors
    /// The `400` response.
    pub fn number(&self, key: &str) -> Result<Option<u64>, Response> {
        self.get(key)
            .map(|value| value.parse::<u64>().map_err(|error| query_rejection(key, &error.to_string())))
            .transpose()
    }

    /// `key` as `true` / `false`; `400` naming the field otherwise.
    ///
    /// # Errors
    /// The `400` response.
    pub fn boolean(&self, key: &str) -> Result<Option<bool>, Response> {
        self.get(key)
            .map(|value| match value {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(query_rejection(key, "provided string was not `true` or `false`")),
            })
            .transpose()
    }
}

/// axum's text for a bad query string, with the field.
fn query_rejection(key: &str, message: &str) -> Response {
    text(StatusCode::BAD_REQUEST, format!("Failed to deserialize query string: {key}: {message}"))
}

/// Percent-decoding with `+` as a space; invalid escapes and UTF-8 are kept as they are.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = |offset: usize| bytes.get(index + offset).and_then(|digit| (*digit as char).to_digit(16));
        let escaped = if byte == b'%' { hex(1).zip(hex(2)) } else { None };
        match (byte, escaped) {
            (_, Some((high, low))) => {
                out.push((high * 16 + low) as u8);
                index += 2;
            }
            (b'+', None) => out.push(b' '),
            _ => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_pairs_are_decoded() {
        let params = QueryParams::parse(Some("from=5&cursor=a%2Fb+c&stream=false&empty&x=%zz"));
        assert_eq!(params.number("from").unwrap(), Some(5));
        assert_eq!(params.get("cursor"), Some("a/b c"));
        assert_eq!(params.boolean("stream").unwrap(), Some(false));
        assert_eq!(params.get("empty"), Some(""));
        assert_eq!(params.get("x"), Some("%zz"));
        assert_eq!(params.number("missing").unwrap(), None);
        assert_eq!(params.number("cursor").unwrap_err().status(), StatusCode::BAD_REQUEST);
        assert_eq!(params.boolean("from").unwrap_err().status(), StatusCode::BAD_REQUEST);
        assert!(QueryParams::parse(None).get("from").is_none());
    }

    #[test]
    fn json_content_types() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, value.parse().unwrap());
            is_json_content_type(&headers)
        };
        assert!(with("application/json"));
        assert!(with("application/json; charset=utf-8"));
        assert!(with("application/cloudevents+json"));
        assert!(!with("text/json"));
        assert!(!with("application/jsonx"));
        assert!(!is_json_content_type(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn json_rejections_match_axum() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Body {
            inputs: std::collections::BTreeMap<String, u32>,
        }
        async fn extract(content_type: Option<&str>, body: &'static str) -> Result<JsonBody<Body>, Response> {
            let mut request = Request::builder().method("POST").uri("/");
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            JsonBody::<Body>::from_request(request.body(axum::body::Body::from(body)).unwrap(), &()).await
        }
        async fn body_text(response: Response) -> (StatusCode, String) {
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16).await.unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
        assert!(extract(Some("application/json"), r#"{"inputs":{"a":1}}"#).await.is_ok());
        let (status, _) = body_text(extract(None, "{}").await.unwrap_err()).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (status, message) = body_text(extract(Some("application/json"), "{").await.unwrap_err()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.starts_with("Failed to parse the request body as JSON: "), "{message}");
        let (status, message) =
            body_text(extract(Some("application/json"), r#"{"inputs":{"a":"x"}}"#).await.unwrap_err()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            message.starts_with("Failed to deserialize the JSON body into the target type: inputs.a: invalid type"),
            "{message}"
        );
    }
}
