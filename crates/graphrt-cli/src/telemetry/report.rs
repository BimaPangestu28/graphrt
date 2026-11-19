//! The telemetry payload and how it is sent.
//!
//! The payload is built only from the fields below: never workflow content,
//! inputs, outputs, file paths, host names or addresses.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use super::state::DayCounts;

/// Payload schema version.
pub const SCHEMA: u32 = 1;
/// Total time allowed for one report request.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Placeholder shown by `graphrt telemetry status` before an id exists.
pub const PENDING_INSTALL_ID: &str = "<created when telemetry is enabled>";

/// One report: what the collector receives, at most once a day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: u32,
    /// Random id of this installation (not derived from the machine or the user).
    pub install_id: String,
    pub version: &'static str,
    /// `std::env::consts::OS`, e.g. `linux`, `macos`, `windows`.
    pub os: &'static str,
    /// `std::env::consts::ARCH`, e.g. `x86_64`, `aarch64`.
    pub arch: &'static str,
    /// `cli` or `docker` (the build-time `GRAPHRT_BUILD`). The embedded library never reports.
    pub build: &'static str,
    /// Cargo features compiled into this binary.
    pub features: Vec<&'static str>,
    /// Complete UTC days not reported yet.
    pub days: Vec<DayReport>,
}

/// Counts of one UTC day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DayReport {
    pub date: String,
    pub runs: u64,
    pub serve_requests: u64,
}

impl Report {
    pub fn new(install_id: String, days: &BTreeMap<String, DayCounts>) -> Self {
        Self {
            schema: SCHEMA,
            install_id,
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            build: build_kind(),
            features: crate::enabled_features(),
            days: days
                .iter()
                .map(|(date, counts)| DayReport {
                    date: date.clone(),
                    runs: counts.runs,
                    serve_requests: counts.serve_requests,
                })
                .collect(),
        }
    }
}

/// The distribution this binary was built for (`GRAPHRT_BUILD=docker` in the Dockerfile).
pub fn build_kind() -> &'static str {
    match option_env!("GRAPHRT_BUILD") {
        Some("docker") => "docker",
        _ => "cli",
    }
}

/// Whether `url` is plain HTTP to this machine (a local collector, `wrangler dev`).
fn is_local_http(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    url.scheme() == "http"
        && (host == "localhost" || unbracketed.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()))
}

/// Validate an endpoint: `https://`, or `http://` to localhost for testing a collector.
///
/// @throws String - When the URL does not parse or uses another scheme.
pub fn parse_endpoint(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid telemetry endpoint `{raw}`: {e}"))?;
    if url.scheme() == "https" || is_local_http(&url) {
        Ok(url)
    } else {
        Err(format!("telemetry endpoint `{raw}` must use https"))
    }
}

/// POST `report` to `url` (blocking; call from a background thread).
///
/// Goes through the HTTP egress policy: private networks are refused, except a
/// plain-HTTP localhost endpoint. Redirects are not followed.
///
/// @throws String - On network errors, timeouts and non-2xx responses.
pub fn send(url: &reqwest::Url, report: &Report) -> Result<(), String> {
    let body = serde_json::to_vec(report).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let policy = graphrt_http::EgressPolicy {
            allow_private: is_local_http(url),
            max_redirects: 0,
            connect_timeout: SEND_TIMEOUT,
            read_timeout: SEND_TIMEOUT,
            ..graphrt_http::EgressPolicy::default()
        };
        let client = graphrt_http::HttpClient::new(policy).map_err(|e| e.to_string())?;
        client.check_url(url).map_err(|e| e.to_string())?;
        let response = client
            .manual_redirect_client(true)
            .map_err(|e| e.to_string())?
            .post(url.clone())
            .timeout(SEND_TIMEOUT)
            .header("content-type", "application/json")
            .header("user-agent", concat!("graphrt/", env!("CARGO_PKG_VERSION")))
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() { Ok(()) } else { Err(format!("HTTP {}", response.status().as_u16())) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_only_the_documented_fields() {
        let mut days = BTreeMap::new();
        days.insert("2026-11-18".to_owned(), DayCounts { runs: 4, serve_requests: 120 });
        let report = Report::new("0123456789abcdef0123456789abcdef".into(), &days);
        let json = serde_json::to_value(&report).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["schema", "install_id", "version", "os", "arch", "build", "features", "days"]);
        assert_eq!(json["days"][0], serde_json::json!({ "date": "2026-11-18", "runs": 4, "serve_requests": 120 }));
        assert_eq!(json["build"], "cli");
    }

    #[test]
    fn endpoints_must_be_https_or_local() {
        assert!(parse_endpoint("https://t.example.com/v1/report").is_ok());
        assert!(parse_endpoint("http://localhost:8787/v1/report").is_ok());
        assert!(parse_endpoint("http://127.0.0.1:8787/v1/report").is_ok());
        assert!(parse_endpoint("http://t.example.com/v1/report").is_err());
        assert!(parse_endpoint("ftp://t.example.com").is_err());
        assert!(parse_endpoint("not a url").is_err());
    }
}
