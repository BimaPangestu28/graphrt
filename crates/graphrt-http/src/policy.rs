//! Egress policy and SSRF classification.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use url::{Host, Url};

/// Rules for outbound HTTP requests made by workflow nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Allow loopback, private, link-local and other non-public addresses.
    /// Off by default: workflows are untrusted input.
    pub allow_private: bool,
    /// If non-empty, only these hosts may be contacted. Entries match the host exactly,
    /// or any subdomain when written as `*.example.com` or `.example.com`.
    pub allowed_hosts: Vec<String>,
    /// Hosts that may never be contacted (same matching rules). Checked before `allowed_hosts`.
    pub denied_hosts: Vec<String>,
    /// Permitted URL schemes.
    pub allowed_schemes: Vec<String>,
    /// Maximum redirects followed (each hop is re-validated). 0 disables redirects.
    pub max_redirects: usize,
    /// Maximum response body size in bytes.
    pub max_response_bytes: usize,
    pub connect_timeout: Duration,
    /// Maximum idle time between reads.
    pub read_timeout: Duration,
    /// Default total request timeout when a node does not configure one.
    pub total_timeout: Duration,
    /// Optional egress proxy URL. Environment proxy variables are always ignored.
    pub proxy: Option<String>,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            allow_private: false,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
            allowed_schemes: vec!["http".to_owned(), "https".to_owned()],
            max_redirects: 5,
            max_response_bytes: 10 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(300),
            proxy: None,
        }
    }
}

/// Reason a destination was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EgressError {
    #[error("invalid URL `{0}`")]
    InvalidUrl(String),
    #[error("URL scheme `{0}` is not allowed")]
    Scheme(String),
    #[error("URL has no host")]
    NoHost,
    #[error("host `{0}` is not allowed by the egress policy")]
    HostDenied(String),
    #[error("address {0} is not a public address (private/loopback/link-local/reserved)")]
    BlockedAddress(IpAddr),
}

impl EgressPolicy {
    /// A policy for trusted, operator-configured endpoints (e.g. a code sandbox):
    /// private addresses allowed, everything else default.
    pub fn trusted() -> Self {
        Self { allow_private: true, ..Self::default() }
    }

    /// Validate a URL before sending a request or following a redirect.
    pub fn check_url(&self, url: &Url) -> Result<(), EgressError> {
        let scheme = url.scheme();
        if !self.allowed_schemes.iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
            return Err(EgressError::Scheme(scheme.to_owned()));
        }
        let host = url.host().ok_or(EgressError::NoHost)?;
        let host_text = match &host {
            Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
            Host::Ipv4(ip) => ip.to_string(),
            Host::Ipv6(ip) => ip.to_string(),
        };
        self.check_host_name(&host_text)?;
        match host {
            Host::Ipv4(ip) => self.check_ip(IpAddr::V4(ip)),
            Host::Ipv6(ip) => self.check_ip(IpAddr::V6(ip)),
            // Domain names are checked after resolution by the guarded resolver.
            Host::Domain(_) => Ok(()),
        }
    }

    /// Check a host name against the allow / deny lists.
    pub fn check_host_name(&self, host: &str) -> Result<(), EgressError> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if self.denied_hosts.iter().any(|p| host_matches(p, &host)) {
            return Err(EgressError::HostDenied(host));
        }
        if !self.allowed_hosts.is_empty() && !self.allowed_hosts.iter().any(|p| host_matches(p, &host)) {
            return Err(EgressError::HostDenied(host));
        }
        Ok(())
    }

    /// Check a resolved or literal IP address.
    pub fn check_ip(&self, ip: IpAddr) -> Result<(), EgressError> {
        if self.allow_private || is_public(ip) { Ok(()) } else { Err(EgressError::BlockedAddress(ip)) }
    }
}

/// `pattern` matches `host` exactly, or as a suffix when written `*.domain` / `.domain`.
fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.").or_else(|| pattern.strip_prefix('.')) {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else {
        host == pattern
    }
}

/// Whether an address is globally routable (safe to contact from a multi-tenant runtime).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = ip.is_unspecified()          // 0.0.0.0
        || a == 0                              // 0.0.0.0/8 "this network"
        || ip.is_loopback()                    // 127.0.0.0/8
        || ip.is_private()                     // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()                  // 169.254/16 (cloud metadata)
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 carrier-grade NAT
        || (a == 192 && b == 0 && c == 0)      // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)      // 192.0.2.0/24 TEST-NET-1
        || (a == 198 && b == 51 && c == 100)   // 198.51.100.0/24 TEST-NET-2
        || (a == 203 && b == 0 && c == 113)    // 203.0.113.0/24 TEST-NET-3
        || (a == 198 && (b == 18 || b == 19))  // 198.18/15 benchmarking
        || (a == 192 && b == 88 && c == 99)    // 6to4 relay anycast
        || ip == Ipv4Addr::new(168, 63, 129, 16) // Azure WireServer (platform agent endpoint)
        || ip.is_multicast()                   // 224/4
        || a >= 240; // 240/4 reserved, incl. 255.255.255.255 broadcast
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // Addresses embedding an IPv4 address are judged by that address.
    if let Some(v4) = embedded_v4(ip) {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    let first = segments.first().copied().unwrap_or(0);
    let blocked = ip.is_unspecified()             // ::
        || ip.is_loopback()                       // ::1
        || ip.is_multicast()                      // ff00::/8
        || (first & 0xfe00) == 0xfc00             // fc00::/7 unique local
        || (first & 0xffc0) == 0xfe80             // fe80::/10 link local
        || (first & 0xffc0) == 0xfec0             // fec0::/10 site local (deprecated)
        || first == 0x2001 && segments.get(1) == Some(&0x0db8) // 2001:db8::/32 documentation
        || first == 0x0100 && segments.get(1..4) == Some(&[0, 0, 0]) // 100::/64 discard
        || first == 0x0064 && segments.get(1..3) == Some(&[0xff9b, 0x0001]) // 64:ff9b:1::/48 local-use NAT64
        || first == 0x2001 && segments.get(1) == Some(&0) // 2001::/32 Teredo (tunnels to arbitrary IPv4)
        || segments.get(..6) == Some(&[0, 0, 0, 0, 0, 0]); // ::/96 deprecated IPv4-compatible
    !blocked
}

/// IPv4 address carried inside an IPv6 address: IPv4-mapped (`::ffff:a.b.c.d`),
/// IPv4-translated (`::ffff:0:a.b.c.d`), NAT64 (`64:ff9b::/96`), 6to4 (`2002::/16`)
/// and IPv4-compatible (`::a.b.c.d`).
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    let from_pair = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    match s {
        [0x0064, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(from_pair(hi, lo)),
        [0, 0, 0, 0, 0xffff, 0, hi, lo] => Some(from_pair(hi, lo)),
        [0x2002, hi, lo, ..] => Some(from_pair(hi, lo)),
        [0, 0, 0, 0, 0, 0, hi, lo] if (hi, lo) != (0, 0) && (hi, lo) != (0, 1) => Some(from_pair(hi, lo)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classification_table() {
        let blocked = [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "198.18.0.1",
            "198.19.255.255",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:0:7f00:1",
            "::ffff:0:a9fe:a9fe",
            "64:ff9b:1::a00:1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "168.63.129.16",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
            "fc00::1",
            "fd00::1",
            "fd00:ec2::254",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "100::1",
        ];
        for s in blocked {
            assert!(!is_public(ip(s)), "{s} should be blocked");
        }
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "169.253.0.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::",
        ];
        for s in public {
            assert!(is_public(ip(s)), "{s} should be public");
        }
    }

    #[test]
    fn literal_urls_are_checked() {
        let p = EgressPolicy::default();
        let check = |u: &str| p.check_url(&Url::parse(u).unwrap());
        assert!(check("http://127.0.0.1/").is_err());
        assert!(check("http://[::1]:8080/").is_err());
        assert!(check("http://[::ffff:7f00:1]/").is_err());
        // Alternative IPv4 notations are normalized by the URL parser.
        assert!(check("http://2130706433/").is_err());
        assert!(check("http://0x7f.1/").is_err());
        assert!(check("http://0177.0.0.1/").is_err());
        assert!(check("http://169.254.169.254/latest/meta-data").is_err());
        assert!(check("ftp://example.com/").is_err());
        assert!(check("file:///etc/passwd").is_err());
        assert!(check("https://example.com/").is_ok());
        assert!(check("http://8.8.8.8/").is_ok());
        let open = EgressPolicy::trusted();
        assert!(open.check_url(&Url::parse("http://127.0.0.1/").unwrap()).is_ok());
    }

    #[test]
    fn host_lists() {
        let p = EgressPolicy {
            allowed_hosts: vec!["*.example.com".into(), "api.other.org".into()],
            denied_hosts: vec!["evil.example.com".into()],
            ..EgressPolicy::default()
        };
        assert!(p.check_host_name("example.com").is_ok());
        assert!(p.check_host_name("a.b.example.com").is_ok());
        assert!(p.check_host_name("API.other.org.").is_ok());
        assert!(p.check_host_name("evil.example.com").is_err());
        assert!(p.check_host_name("other.org").is_err());
        assert!(p.check_host_name("notexample.com").is_err());
    }
}
