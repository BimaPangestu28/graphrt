//! Configuration from the standard `OTEL_*` environment variables.

use graphrt_core::urls::{self, redact_userinfo};

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

/// Exporter settings resolved from environment variables. `Debug` shows header
/// names only and endpoints without userinfo.
#[derive(Clone, PartialEq)]
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
    /// What content (inputs, messages, outputs, tool arguments...) spans carry:
    /// `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`, or `SPAN_ONLY` for the older
    /// `GRAPHRT_OTEL_CAPTURE_CONTENT=true`. Off by default.
    pub capture: CaptureMode,
    /// Push metrics to `metrics_endpoint` (off with a vendor preset: none of
    /// them ingests OTLP metrics).
    pub export_metrics: bool,
    /// The vendor preset applied (`GRAPHRT_OTEL_PRESET`), see [`crate::preset`].
    pub preset: Option<&'static str>,
}

/// Content capture policy (`OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`).
///
/// Content means run inputs and outputs, rendered model messages, tool
/// arguments and results, and retrieval queries and documents. Secrets stay
/// masked and each value is capped in size either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    /// No content (the default).
    #[default]
    NoContent,
    /// Content as span attributes.
    SpanOnly,
    /// Content on a `gen_ai.client.inference.operation.details` span event.
    EventOnly,
    /// Both.
    SpanAndEvent,
}

impl CaptureMode {
    /// Parse the standard variable's value, case-insensitively. Unknown values
    /// mean [`CaptureMode::NoContent`], as the conventions require.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "SPAN_ONLY" => CaptureMode::SpanOnly,
            "EVENT_ONLY" => CaptureMode::EventOnly,
            "SPAN_AND_EVENT" => CaptureMode::SpanAndEvent,
            _ => CaptureMode::NoContent,
        }
    }

    /// Whether any content is captured.
    pub fn is_enabled(self) -> bool {
        self != CaptureMode::NoContent
    }

    /// Whether content goes into span attributes.
    pub fn on_span(self) -> bool {
        matches!(self, CaptureMode::SpanOnly | CaptureMode::SpanAndEvent)
    }

    /// Whether content goes into span events.
    pub fn on_event(self) -> bool {
        matches!(self, CaptureMode::EventOnly | CaptureMode::SpanAndEvent)
    }
}

impl std::fmt::Debug for OtelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("OtelConfig")
            .field("disabled", &self.disabled)
            .field("endpoint_configured", &self.endpoint_configured)
            .field("traces_endpoint", &redact_userinfo(&self.traces_endpoint))
            .field("metrics_endpoint", &redact_userinfo(&self.metrics_endpoint))
            .field("headers", &header_names)
            .field("resource", &self.resource)
            .field("capture", &self.capture)
            .field("export_metrics", &self.export_metrics)
            .field("preset", &self.preset)
            .finish()
    }
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
            capture: CaptureMode::NoContent,
            export_metrics: true,
            preset: None,
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
    /// `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT`, and
    /// `GRAPHRT_OTEL_CAPTURE_CONTENT` (an older alias: `true` means `SPAN_ONLY`),
    /// and `GRAPHRT_OTEL_PRESET` (a vendor [`crate::preset`]: its endpoint,
    /// headers and resource attributes, under explicit `OTEL_*` values).
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

        let preset = match get(crate::preset::PRESET_ENV) {
            Some(name) => {
                let preset = crate::preset::find(name)?;
                Some((preset.name, preset.resolve(vars)?))
            }
            None => None,
        };
        if let Some((name, resolved)) = &preset {
            cfg.preset = Some(*name);
            cfg.endpoint_configured = true;
            if get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_none() {
                cfg.traces_endpoint.clone_from(&resolved.traces_endpoint);
            }
            cfg.export_metrics = get("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_some();
            cfg.headers.clone_from(&resolved.headers);
        }
        if let Some(raw) = get("OTEL_EXPORTER_OTLP_HEADERS") {
            for (k, v) in parse_pairs("OTEL_EXPORTER_OTLP_HEADERS", raw)? {
                cfg.headers.retain(|(name, _)| !name.eq_ignore_ascii_case(&k));
                cfg.headers.push((k, v));
            }
        }
        if let Some(raw) = get("OTEL_EXPORTER_OTLP_TRACES_HEADERS") {
            for (k, v) in parse_pairs("OTEL_EXPORTER_OTLP_TRACES_HEADERS", raw)? {
                cfg.headers.retain(|(name, _)| !name.eq_ignore_ascii_case(&k));
                cfg.headers.push((k, v));
            }
        }
        if let Some((name, resolved)) = &preset {
            cfg.check_preset_credentials(name, resolved)?;
        }

        let mut resource = Resource::default();
        for (k, v) in preset.iter().flat_map(|(_, resolved)| &resolved.resource) {
            resource = resource.with(k, AttrValue::Str(v.clone()));
        }
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
        cfg.capture = match get("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT") {
            Some(mode) => CaptureMode::parse(mode),
            None if get("GRAPHRT_OTEL_CAPTURE_CONTENT").is_some_and(is_true) => CaptureMode::SpanOnly,
            None => CaptureMode::NoContent,
        };
        Ok(cfg)
    }
}

impl OtelConfig {
    /// Where the export headers go: the traces endpoint, and the metrics one
    /// when metrics are pushed.
    fn header_destinations(&self) -> Vec<(&'static str, &str)> {
        let mut destinations = vec![("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", self.traces_endpoint.as_str())];
        if self.export_metrics {
            destinations.push(("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", self.metrics_endpoint.as_str()));
        }
        destinations
    }

    /// Refuse to send a preset's credential headers anywhere but the preset's
    /// own host, and never in clear text to another machine.
    ///
    /// An explicit endpoint variable overriding the preset's (a collector, a
    /// typo, a variable left over from another setup) would otherwise receive
    /// the vendor key. Credentials the operator set explicitly in
    /// `OTEL_EXPORTER_OTLP_HEADERS` replace the preset's and are not checked:
    /// that is the documented escape hatch.
    ///
    /// @throws ConfigError - When a preset credential would go to another host,
    ///   or over `http://` to a non-loopback host.
    fn check_preset_credentials(&self, preset: &str, resolved: &crate::preset::Resolved) -> Result<(), ConfigError> {
        let carries_preset_credential = self.headers.iter().any(|header| {
            crate::preset::is_credential_header(&header.0) && resolved.headers.iter().any(|own| own == header)
        });
        if !carries_preset_credential {
            return Ok(());
        }
        let preset_origin = urls::origin(&resolved.traces_endpoint);
        for (var, endpoint) in self.header_destinations() {
            if urls::origin(endpoint) != preset_origin {
                return Err(ConfigError::Invalid {
                    var: var.to_owned(),
                    message: format!(
                        "points at {}, not the `{preset}` preset's host {}; its credentials are only sent \
                         there. Set the preset's base URL variable instead, or put the credentials in \
                         OTEL_EXPORTER_OTLP_HEADERS yourself",
                        display_host(endpoint),
                        display_host(&resolved.traces_endpoint),
                    ),
                });
            }
            if urls::is_cleartext_remote(endpoint) {
                return Err(ConfigError::Invalid {
                    var: format!("{preset} preset URL"),
                    message: format!(
                        "{} is plain http:// to another machine; the preset's credentials need https:// \
                         (or a loopback address)",
                        redact_userinfo(endpoint)
                    ),
                });
            }
        }
        Ok(())
    }
}

fn display_host(url: &str) -> String {
    urls::host(url).unwrap_or_else(|| "an invalid URL".to_owned())
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
    fn debug_hides_header_values() {
        let cfg =
            OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_HEADERS", "x-honeycomb-team=hc-secret-key")])).unwrap();
        let shown = format!("{cfg:?}");
        assert!(shown.contains("x-honeycomb-team") && !shown.contains("hc-secret-key"), "{shown}");
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
        assert!(cfg.endpoint_configured);
        assert_eq!(cfg.capture, CaptureMode::SpanOnly);
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
    fn standard_capture_variable_wins_over_the_alias() {
        let mode = |pairs: &[(&str, &str)]| OtelConfig::from_vars(&vars(pairs)).unwrap().capture;
        assert_eq!(mode(&[]), CaptureMode::NoContent);
        assert_eq!(mode(&[("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT", "event_only")]), CaptureMode::EventOnly);
        assert_eq!(
            mode(&[("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT", "SPAN_AND_EVENT")]),
            CaptureMode::SpanAndEvent
        );
        assert_eq!(mode(&[("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT", "bogus")]), CaptureMode::NoContent);
        let both =
            [("GRAPHRT_OTEL_CAPTURE_CONTENT", "true"), ("OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT", "NO_CONTENT")];
        assert_eq!(mode(&both), CaptureMode::NoContent);
        assert_eq!(mode(&[("GRAPHRT_OTEL_CAPTURE_CONTENT", "1")]), CaptureMode::SpanOnly);
        assert!(CaptureMode::SpanAndEvent.on_span() && CaptureMode::SpanAndEvent.on_event());
        assert!(!CaptureMode::EventOnly.on_span() && !CaptureMode::NoContent.is_enabled());
    }

    #[test]
    fn a_preset_fills_endpoint_headers_and_resource() {
        let cfg = OtelConfig::from_vars(&vars(&[
            ("GRAPHRT_OTEL_PRESET", "opik"),
            ("OPIK_API_KEY", "op-key"),
            ("OPIK_PROJECT_NAME", "agents"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-extra=1,authorization=override"),
            ("OTEL_SERVICE_NAME", "svc"),
        ]))
        .unwrap();
        assert_eq!(cfg.preset, Some("opik"));
        assert!(cfg.endpoint_configured && !cfg.export_metrics);
        assert_eq!(cfg.traces_endpoint, "https://www.comet.com/opik/api/v1/private/otel/v1/traces");
        let header = |name: &str| cfg.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(header("authorization"), Some("override"), "explicit OTEL headers win");
        assert_eq!(header("x-extra"), Some("1"));
        assert_eq!(header("projectname"), Some("agents"));
        let attrs = &cfg.resource.attributes;
        assert!(attrs.contains(&("opik.project_name".into(), AttrValue::from("agents"))));
        assert!(attrs.contains(&("service.name".into(), AttrValue::from("svc"))));

        let cfg = OtelConfig::from_vars(&vars(&[
            ("GRAPHRT_OTEL_PRESET", "mlflow"),
            ("MLFLOW_EXPERIMENT_ID", "3"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://collector:4318/v1/traces"),
            ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://collector:4318/v1/metrics"),
        ]))
        .unwrap();
        assert_eq!(cfg.traces_endpoint, "http://collector:4318/v1/traces");
        assert!(cfg.export_metrics);
        assert!(OtelConfig::from_vars(&vars(&[("GRAPHRT_OTEL_PRESET", "mlflow")])).is_err());
        assert!(OtelConfig::from_vars(&vars(&[("GRAPHRT_OTEL_PRESET", "nope")])).is_err());
        assert!(OtelConfig::from_vars(&[]).unwrap().export_metrics);
    }

    #[test]
    fn preset_credentials_stay_on_the_preset_host() {
        let langfuse =
            [("GRAPHRT_OTEL_PRESET", "langfuse"), ("LANGFUSE_PUBLIC_KEY", "pk"), ("LANGFUSE_SECRET_KEY", "sk")];
        let with = |extra: &[(&str, &str)]| {
            let mut pairs = langfuse.to_vec();
            pairs.extend_from_slice(extra);
            OtelConfig::from_vars(&vars(&pairs))
        };
        // Another host for traces or pushed metrics: refused, naming the variable.
        let err = with(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://collector.example/v1/traces")]).unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") && shown.contains("cloud.langfuse.com"),
            "{shown}"
        );
        assert!(with(&[("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "https://metrics.example/v1/metrics")]).is_err());
        // The preset's own host, by any spelling, is fine.
        let same = "https://Cloud.Langfuse.com:443/api/public/otel/v1/traces";
        assert!(with(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", same)]).is_ok());
        // Explicit credentials replace the preset's: the operator's own choice.
        let explicit = [
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://collector.example/v1/traces"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer%20collector-key"),
        ];
        assert!(with(&explicit).is_ok());
        // Plain http to another machine: refused; loopback is fine.
        let err = with(&[("LANGFUSE_BASE_URL", "http://lf.internal:3000")]).unwrap_err();
        assert!(err.to_string().contains("plain http://"), "{err}");
        assert!(with(&[("LANGFUSE_BASE_URL", "http://127.0.0.1:3000")]).is_ok());
        // Presets without a credential header set are not restricted.
        let phoenix = OtelConfig::from_vars(&vars(&[
            ("GRAPHRT_OTEL_PRESET", "phoenix"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://px.internal:6006/v1/traces"),
        ]));
        assert!(phoenix.is_ok());
    }

    #[test]
    fn debug_hides_url_credentials() {
        let cfg = OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://user:hunter2@otel.example")]))
            .unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("hunter2") && shown.contains("***@otel.example"), "{shown}");
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_HEADERS", "novalue")])).is_err());
        assert!(OtelConfig::from_vars(&vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://x")])).is_err());
    }
}
