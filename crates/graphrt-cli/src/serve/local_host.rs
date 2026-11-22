//! DNS rebinding protection of a keyless loopback server: only loopback `Host`
//! names are accepted.

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use super::errors::error_response;

/// Reject requests whose `Host` is not a loopback name (DNS rebinding protection for
/// an unauthenticated loopback server).
pub(super) async fn require_local_host(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()));
    if host.as_deref().is_some_and(is_local_host) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::FORBIDDEN,
            "requests to an unauthenticated server must use a loopback host name (localhost, 127.0.0.1, [::1]); \
             set an API key to serve other host names",
        )
    }
}

/// Whether a `Host` header value names this machine (`localhost`, `*.localhost`,
/// `127.x.y.z`, `[::1]`), with or without a port.
fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:8080`
        match rest.split_once(']') {
            Some((inside, port)) if port.is_empty() || port.starts_with(':') => inside,
            _ => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::is_local_host;

    #[test]
    fn local_host_names() {
        for host in ["localhost", "localhost:8080", "LOCALHOST.", "app.localhost:3000", "127.0.0.1", "127.0.0.1:8080"] {
            assert!(is_local_host(host), "{host}");
        }
        for host in ["[::1]", "[::1]:8080", "127.1.2.3:80"] {
            assert!(is_local_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8080",
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "[::1]x",
            "10.0.0.1:8080",
            "",
        ] {
            assert!(!is_local_host(host), "{host}");
        }
    }
}
