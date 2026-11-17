//! Configuration from the standard `OTEL_*` environment variables.

use crate::otlp::Resource;
use crate::span::AttrValue;

/// Default OTLP/HTTP endpoint (a local collector).
pub const DEFAULT_ENDPOINT: &str = "http://localhost:4318";

/// Configuration error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("invalid {var}: {message}")]
    Invalid { var: String, message: String },
}

/// Exporter settings resolved from environment variables.
#[derive(Clone, Debug, PartialEq)]
pub struct OtelConfig {
    /// `OTEL_SDK_DISABLED=true`.
    pub disabled: bool,
    /// Whether any OTLP endpoint variable was set explicitly.
    pub endpoint_configured: bool,
    /// Full URL for trace export (`.../v1/traces`).
    pub traces_endpoint: String,
    /// Full URL for metric export (`.../v1/metrics`).
    pub metrics_endpoint: String,
    /// Extra request headers (e.g. vendor API keys). Values are secrets: never logged.
    pub headers: Vec<(String, String)>,
    pub resource: Resource,
    /// Attach truncated node outputs to spans (`GRAPHRT_OTEL_CAPTURE_CONTENT=true`).
    pub capture_content: bool,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            endpoint_configured: false,
            traces_endpoint: format!("{DEFAULT_ENDPOINT}/v1/traces"),
            metrics_endpoint: format!("{DEFAULT_ENDPOINT}/v1/metrics"),
            headers: Vec::new(),
            resource: Resource::default(),
            capture_content: false,
        }
    }
}

impl OtelConfig {
    /// Read the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        Self::from_vars(&vars)
    }

    /// Resolve from explicit variables. Supported: `OTEL_SDK_DISABLED`,
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`,
    /// `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
    /// `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, `OTEL_SERVICE_NAME`,
    /// `OTEL_RESOURCE_ATTRIBUTES`, and `GRAPHRT_OTEL_CAPTURE_CONTENT`.
    pub fn from_vars(vars: &[(String, String)]) -> Result<Self, ConfigError> {
        let get = |k: &str| vars.iter().rev().find(|(n, v)| n == k && !v.trim().is_empty()).map(|(_, v)| v.trim());
        let mut cfg = Self { disabled: get("OTEL_SDK_DISABLED").is_some_and(is_true), ..Self::default() };

        let base = get("OTEL_EXPORTER_OTLP_ENDPOINT");
        if let Some(base) = base {
            let base = base.trim_end_matches('/');
            cfg.traces_endpoint = format!("{base}/v1/traces");
            cfg.metrics_endpoint = format!("{base}/v1/metrics");
            cfg.endpoint_configured = true;
        }
        // Signal-specific endpoints are used as-is (the spec appends no path to them).
        if let Some(url) = get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
            cfg.traces_endpoint = url.to_owned();
            cfg.endpoint_configured = true;
        }
        if let Some(url) = get("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
            cfg.metrics_endpoint = url.to_owned();
        }
        for (var, url) in [("traces endpoint", &cfg.traces_endpoint), ("metrics endpoint", &cfg.metrics_endpoint)] {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ConfigError::Invalid { var: var.into(), message: "must be an http(s) URL".into() });
            }
        }

        if let Some(raw) = get("OTEL_EXPORTER_OTLP_HEADERS") {
            cfg.headers = parse_pairs("OTEL_EXPORTER_OTLP_HEADERS", raw)?;
        }
        if let Some(raw) = get("OTEL_EXPORTER_OTLP_TRACES_HEADERS") {
            for (k, v) in parse_pairs("OTEL_EXPORTER_OTLP_TRACES_HEADERS", raw)? {
                cfg.headers.retain(|(name, _)| !name.eq_ignore_ascii_case(&k));
                cfg.headers.push((k, v));
            }
        }

        let mut resource = Resource::default();
        if let Some(raw) = get("OTEL_RESOURCE_ATTRIBUTES") {
            for (k, v) in parse_pairs("OTEL_RESOURCE_ATTRIBUTES", raw)? {
                resource = resource.with(&k, AttrValue::Str(v));
            }
        }
        // OTEL_SERVICE_NAME wins over a service.name in OTEL_RESOURCE_ATTRIBUTES.
        if let Some(name) = get("OTEL_SERVICE_NAME") {
            resource = resource.with("service.name", name);
        }
        cfg.resource = resource;
        cfg.capture_content = get("GRAPHRT_OTEL_CAPTURE_CONTENT").is_some_and(is_true);
        Ok(cfg)
    }
}

fn is_true(v: &str) -> bool {
    v.eq_ignore_ascii_case("true") || v == "1"
}

/// Parse `k1=v1,k2=v2` (W3C baggage-like; values are percent-decoded).
pub fn parse_pairs(var: &str, raw: &str) -> Result<Vec<(String, String)>, ConfigError> {
    let mut out = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((k, v)) = item.split_once('=') else {
            return Err(ConfigError::Invalid {
                var: var.to_owned(),
                message: "expected comma-separated key=value pairs".into(),
            });
        };
        let k = percent_decode(k.trim());
        if k.is_empty() {
            return Err(ConfigError::Invalid { var: var.to_owned(), message: "empty key".into() });
        }
        out.push((k, percent_decode(v.trim())));
    }
    Ok(out)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn defaults_point_at_local_collector() {
        let cfg = OtelConfig::from_vars(&[]).unwrap();
        assert!(!cfg.disabled && !cfg.endpoint_configured);
        assert_eq!(cfg.traces_endpoint, "http://localhost:4318/v1/traces");
    }

    #[test]
    fn env_variables_are_honored() {
        let cfg = OtelConfig::from_vars(&vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://otel.example.com/"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=abc%20def, x-team = core"),
            ("OTEL_SERVICE_NAME", "agents"),
            ("OTEL_RESOURCE_ATTRIBUTES", "service.name=ignored,deployment.environment=prod"),
            ("GRAPHRT_OTEL_CAPTURE_CONTENT", "true"),
        ]))
        .unwrap();
        assert!(cfg.endpoint_configured && cfg.capture_content);
        assert_eq!(cfg.traces_endpoint, "https://otel.example.com/v1/traces");
        assert_eq!(cfg.metrics_endpoint, "https://otel.example.com/v1/metrics");
        assert_eq!(cfg.headers, vec![("x-api-key".into(), "abc def".into()), ("x-team".into(), "core".into())]);
        let attrs = &cfg.resource.attributes;
        assert!(attrs.contains(&("service.name".into(), AttrValue::from("agents"))));
        assert!(attrs.contains(&("deployment.environment".into(), AttrValue::from("prod"))));

        let cfg = OtelConfig::from_vars(&vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://a:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://b:9999/custom"),
            ("OTEL_SDK_DISABLED", "TRUE"),
        ]))
        .unwrap();
        assert_eq!(cfg.traces_endpoint, "http://b:9999/custom");
        assert!(cfg.disabled);
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_HEADERS", "novalue")])).is_err());
        assert!(OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://x")])).is_err());
    }
}
