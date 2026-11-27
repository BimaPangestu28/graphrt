//! Cross-origin requests from browsers (`--cors-origin`, cargo feature `cors`).
//!
//! Off unless an origin is given. Then:
//!
//! - Only the listed origins (scheme, host and port, compared exactly) get
//!   `Access-Control-Allow-Origin`, which echoes their origin. `*` allows any
//!   origin and is refused at startup without an API key.
//! - `Access-Control-Allow-Credentials` is never sent. The API authenticates by
//!   tokens in headers, which a page adds itself, so cookies and other ambient
//!   credentials of the browser are never accepted from another origin and a
//!   wildcard can never be combined with them.
//! - A preflight (`OPTIONS` with `Access-Control-Request-Method`) is answered
//!   here, before authentication: browsers send it without the token.
//! - Every answer carries `Vary: Origin`, so a shared cache never serves one
//!   origin's answer to another.
//!
//! A request from an origin that is not listed is not blocked here: it gets no
//! CORS headers, so the browser keeps the answer from the page. It still needs
//! a token the page does not have. Server-sent events are ordinary `fetch`
//! responses and are covered like every other route.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::errors::error_response;
use crate::error::CliError;

/// Fallback of `--cors-origin`: origins separated by commas.
pub const ORIGINS_ENV: &str = "GRAPHRT_CORS_ORIGINS";

/// Request headers a page may always send.
const BASE_HEADERS: &[&str] = &["authorization", "content-type", "last-event-id", "traceparent", "tracestate"];
const ALLOWED_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
/// Response headers a page may read besides the safelisted ones.
const EXPOSED_HEADERS: &str = "retry-after, www-authenticate";
/// How long a browser may cache a preflight answer, in seconds.
const MAX_AGE_SECS: &str = "600";

/// The cross-origin policy of a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cors {
    /// Allowed origins as browsers send them (`https://app.example.com`), lower case.
    origins: Vec<String>,
    /// `*` was given: any origin is allowed.
    any: bool,
    /// The value of `Access-Control-Allow-Headers`.
    allow_headers: HeaderValue,
}

impl Cors {
    /// The policy for `origins` (each `scheme://host[:port]`, or `*`), or `None`
    /// when there are none. `extra_headers` are request headers pages may send
    /// besides the standard ones (the `--input-header` names).
    ///
    /// @throws CliError (usage) - An entry that is not an origin, or `*` without an API key.
    ///
    /// @example
    /// let cors = Cors::new(&["https://app.example.com".to_owned()], ["x-user-authorization"], true)?;
    pub fn new<'a>(
        origins: &[String],
        extra_headers: impl IntoIterator<Item = &'a str>,
        has_api_key: bool,
    ) -> Result<Option<Self>, CliError> {
        if origins.is_empty() {
            return Ok(None);
        }
        let any = origins.iter().any(|origin| origin == "*");
        if any && !has_api_key {
            return Err(CliError::usage(
                "`--cors-origin '*'` needs an API key: without one, any web page a user of this machine opens \
                 could call the API",
            ));
        }
        let mut allowed = Vec::new();
        for origin in origins.iter().filter(|origin| origin.as_str() != "*") {
            allowed.push(normalize_origin(origin).map_err(|why| {
                CliError::usage(format!(
                    "--cors-origin `{origin}`: {why} (expected an origin such as https://app.example.com)"
                ))
            })?);
        }
        let mut headers: Vec<String> = BASE_HEADERS.iter().map(|name| (*name).to_owned()).collect();
        for name in extra_headers {
            let name = name.to_ascii_lowercase();
            if !headers.contains(&name) {
                headers.push(name);
            }
        }
        let allow_headers = HeaderValue::from_str(&headers.join(", "))
            .map_err(|_| CliError::usage("--cors-origin: a mapped header name is not a valid header value"))?;
        Ok(Some(Self { origins: allowed, any, allow_headers }))
    }

    /// The listed origins (without `*`), as browsers send them.
    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        if self.any {
            "cross-origin requests allowed from any origin (tokens in headers only, no cookies)".to_owned()
        } else {
            format!("cross-origin requests allowed from {}", self.origins.join(", "))
        }
    }

    /// The `Access-Control-Allow-Origin` value for a request from `origin`, or
    /// `None` when the origin is not allowed.
    fn allow_origin(&self, origin: &HeaderValue) -> Option<HeaderValue> {
        if self.any {
            return Some(HeaderValue::from_static("*"));
        }
        let text = origin.to_str().ok()?.to_ascii_lowercase();
        self.origins.contains(&text).then(|| origin.clone())
    }
}

/// An origin as a browser sends it: lower case, no path, no default port.
///
/// @throws String - Why `origin` is not one.
fn normalize_origin(origin: &str) -> Result<String, String> {
    let lower = origin.trim().to_ascii_lowercase();
    let (scheme, rest) = lower.split_once("://").ok_or("it has no scheme")?;
    if scheme != "http" && scheme != "https" {
        return Err("the scheme must be http or https".to_owned());
    }
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) || rest.contains(char::is_whitespace) {
        return Err("it must be scheme://host[:port], without a path".to_owned());
    }
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    let host = rest.strip_suffix(default_port).unwrap_or(rest);
    if host.is_empty() {
        return Err("it has no host".to_owned());
    }
    Ok(format!("{scheme}://{host}"))
}

/// Origins from `--cors-origin`, else from `$GRAPHRT_CORS_ORIGINS` (commas).
pub fn origins_from(flags: &[String], env: Option<String>) -> Vec<String> {
    if !flags.is_empty() {
        return flags.to_vec();
    }
    env.unwrap_or_default().split(',').map(str::trim).filter(|origin| !origin.is_empty()).map(str::to_owned).collect()
}

/// The middleware: answers preflights and adds the CORS headers of allowed origins.
pub(super) async fn apply(State(cors): State<Arc<Cors>>, request: Request, next: Next) -> Response {
    let Some(origin) = request.headers().get(ORIGIN).cloned() else {
        // Not a browser's cross-origin request.
        return with_vary(next.run(request).await);
    };
    let allowed = cors.allow_origin(&origin);
    let is_preflight =
        request.method() == Method::OPTIONS && request.headers().contains_key(ACCESS_CONTROL_REQUEST_METHOD);
    if is_preflight {
        return match allowed {
            Some(allow_origin) => preflight(&cors, allow_origin),
            None => with_vary(error_response(StatusCode::FORBIDDEN, "this origin is not allowed (--cors-origin)")),
        };
    }
    let mut response = next.run(request).await;
    if let Some(allow_origin) = allowed {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin);
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSED_HEADERS));
    }
    with_vary(response)
}

/// The answer to a preflight of an allowed origin.
fn preflight(cors: &Cors, allow_origin: HeaderValue) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin);
    headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(ALLOWED_METHODS));
    headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, cors.allow_headers.clone());
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE_SECS));
    with_vary((StatusCode::NO_CONTENT, headers).into_response())
}

/// Add `Vary: Origin`: the answer depends on who asks.
fn with_vary(mut response: Response) -> Response {
    response.headers_mut().append(VARY, HeaderValue::from_static("origin"));
    response
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    fn origins(list: &[&str]) -> Vec<String> {
        list.iter().map(|origin| (*origin).to_owned()).collect()
    }

    fn app(cors: Cors) -> Router {
        Router::new()
            .route("/v1/thing", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(Arc::new(cors), apply))
    }

    async fn send(app: Router, method: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = HttpRequest::builder().method(method).uri("/v1/thing");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.oneshot(request.body(Body::empty()).unwrap()).await.unwrap()
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response.headers().get(name).and_then(|value| value.to_str().ok())
    }

    #[test]
    fn origins_are_normalized_and_checked() {
        assert_eq!(normalize_origin("HTTPS://App.Example.com:443").unwrap(), "https://app.example.com");
        assert_eq!(normalize_origin("http://localhost:5173").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_origin("http://[::1]:3000").unwrap(), "http://[::1]:3000");
        for bad in ["app.example.com", "https://app.example.com/", "https://a.example/path", "ftp://a.example", "null"]
        {
            assert!(normalize_origin(bad).is_err(), "{bad}");
        }
        assert!(Cors::new(&origins(&["https://a.example/x"]), [], true).is_err());
    }

    #[test]
    fn no_origin_means_no_policy_and_a_wildcard_needs_a_key() {
        assert_eq!(Cors::new(&[], [], false).unwrap(), None);
        assert!(Cors::new(&origins(&["*"]), [], false).is_err());
        assert!(Cors::new(&origins(&["*"]), [], true).unwrap().is_some());
    }

    #[test]
    fn flags_win_over_the_environment() {
        assert_eq!(origins_from(&origins(&["https://a.example"]), Some("https://b.example".into())).len(), 1);
        let from_env = origins_from(&[], Some("https://a.example, https://b.example,".into()));
        assert_eq!(from_env, origins(&["https://a.example", "https://b.example"]));
        assert!(origins_from(&[], None).is_empty());
    }

    #[tokio::test]
    async fn an_allowed_origin_is_echoed_without_credentials() {
        let cors = Cors::new(&origins(&["https://app.example.com"]), [], true).unwrap().unwrap();
        let response = send(app(cors), "GET", &[("origin", "https://app.example.com")]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "access-control-allow-origin"), Some("https://app.example.com"));
        assert_eq!(header(&response, "access-control-allow-credentials"), None);
        assert_eq!(header(&response, "vary"), Some("origin"));
    }

    #[tokio::test]
    async fn another_origin_gets_no_cors_headers_and_its_preflight_is_refused() {
        let cors = Cors::new(&origins(&["https://app.example.com"]), [], true).unwrap().unwrap();
        // A look-alike: the allowed origin as a prefix, another scheme, another port.
        for origin in
            ["https://app.example.com.evil.example", "http://app.example.com", "https://app.example.com:8443", "null"]
        {
            let response = send(app(cors.clone()), "GET", &[("origin", origin)]).await;
            assert_eq!(header(&response, "access-control-allow-origin"), None, "{origin}");
            assert_eq!(header(&response, "vary"), Some("origin"));
            let preflight = [("origin", origin), ("access-control-request-method", "POST")];
            let response = send(app(cors.clone()), "OPTIONS", &preflight).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
            assert_eq!(header(&response, "access-control-allow-origin"), None, "{origin}");
        }
    }

    #[tokio::test]
    async fn a_preflight_is_answered_before_the_route_with_the_mapped_headers() {
        let cors = Cors::new(&origins(&["http://localhost:5173"]), ["X-User-Authorization"], true).unwrap().unwrap();
        let preflight = [
            ("origin", "http://localhost:5173"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "authorization, content-type, x-user-authorization"),
        ];
        let response = send(app(cors), "OPTIONS", &preflight).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(header(&response, "access-control-allow-origin"), Some("http://localhost:5173"));
        let allowed = header(&response, "access-control-allow-headers").unwrap();
        for name in ["authorization", "content-type", "last-event-id", "x-user-authorization"] {
            assert!(allowed.split(", ").any(|header| header == name), "{name} in {allowed}");
        }
        assert!(header(&response, "access-control-allow-methods").unwrap().contains("DELETE"));
        assert_eq!(header(&response, "access-control-max-age"), Some("600"));
        assert_eq!(header(&response, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn a_wildcard_answers_with_a_star_and_never_with_credentials() {
        let cors = Cors::new(&origins(&["*"]), [], true).unwrap().unwrap();
        let response = send(app(cors), "GET", &[("origin", "https://anything.example")]).await;
        assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&response, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn requests_without_an_origin_pass_untouched() {
        let cors = Cors::new(&origins(&["https://app.example.com"]), [], true).unwrap().unwrap();
        let response = send(app(cors), "GET", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "access-control-allow-origin"), None);
    }
}
