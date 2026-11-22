//! Small, allocation-light helpers for operator-configured URLs: showing them
//! without credentials, and comparing where they point.
//!
//! A URL such as `https://user:token@collector.example/v1/traces` carries a
//! credential in its userinfo. Hosts print URLs in startup lines, errors and
//! `Debug` output, so they pass them through [`redact_userinfo`] first. The
//! parsing here is deliberately simple (scheme, authority, rest): it is for
//! display and same-host checks of `http(s)` URLs, not a full RFC 3986 parser.

use std::borrow::Cow;

/// Replacement shown for a URL's userinfo.
pub const REDACTED_USERINFO: &str = "***";

/// `url` with any `user:password@` replaced by `***@`.
///
/// @example
/// assert_eq!(redact_userinfo("https://u:p@h.example/x"), "https://***@h.example/x");
pub fn redact_userinfo(url: &str) -> Cow<'_, str> {
    let Some((scheme, rest)) = url.split_once("://") else { return Cow::Borrowed(url) };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_, host)) => Cow::Owned(format!("{scheme}://{REDACTED_USERINFO}@{host}{path}")),
        None => Cow::Borrowed(url),
    }
}

/// The host of an `http(s)` URL: lowercase, without userinfo, port or IPv6 brackets.
pub fn host(url: &str) -> Option<String> {
    let (host, _) = host_and_port(url)?;
    Some(host)
}

/// `scheme://host:port` of an `http(s)` URL, lowercase, with the default port
/// filled in, so `https://A.example` and `https://a.example:443/x` compare equal.
pub fn origin(url: &str) -> Option<String> {
    let scheme = scheme(url)?;
    let (host, port) = host_and_port(url)?;
    let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some(format!("{scheme}://{host}:{port}"))
}

/// Whether `host` names this machine: `localhost` or a loopback IP address.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether `url` is plain `http://` to a host other than this machine, where
/// credentials would cross the network in clear text.
pub fn is_cleartext_remote(url: &str) -> bool {
    scheme(url).as_deref() == Some("http") && !host(url).is_some_and(|host| is_loopback_host(&host))
}

fn scheme(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once("://")?;
    Some(scheme.to_ascii_lowercase())
}

fn host_and_port(url: &str) -> Option<(String, Option<u16>)> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (inside, after) = bracketed.split_once(']')?;
        (inside, after.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(text) => Some(text.parse::<u16>().ok()?),
        None => None,
    };
    Some((host.to_ascii_lowercase(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_redacted() {
        assert_eq!(redact_userinfo("https://u:p@h.example/x?y=1"), "https://***@h.example/x?y=1");
        assert_eq!(redact_userinfo("http://token@127.0.0.1:3000"), "http://***@127.0.0.1:3000");
        assert_eq!(redact_userinfo("https://h.example/a@b"), "https://h.example/a@b");
        assert_eq!(redact_userinfo("not a url"), "not a url");
    }

    #[test]
    fn origins_compare_hosts_and_ports() {
        assert_eq!(origin("https://A.example/x"), origin("https://a.example:443/y"));
        assert_ne!(origin("https://a.example"), origin("https://b.example"));
        assert_ne!(origin("http://a.example"), origin("https://a.example"));
        assert_eq!(origin("http://u:p@[::1]:8080/x").as_deref(), Some("http://::1:8080"));
        assert_eq!(origin("http://a.example:notaport"), None);
        assert_eq!(host("https://user@Cloud.Example:9/x").as_deref(), Some("cloud.example"));
    }

    #[test]
    fn cleartext_to_remote_hosts() {
        assert!(is_cleartext_remote("http://langfuse.internal:3000"));
        assert!(!is_cleartext_remote("http://localhost:3000"));
        assert!(!is_cleartext_remote("http://127.0.0.1:3000"));
        assert!(!is_cleartext_remote("http://[::1]:3000"));
        assert!(!is_cleartext_remote("https://cloud.langfuse.com"));
        assert!(!is_loopback_host("localhost.evil.example"));
    }
}
