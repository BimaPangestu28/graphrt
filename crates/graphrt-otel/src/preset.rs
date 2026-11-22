//! Vendor export presets (`GRAPHRT_OTEL_PRESET`, `graphrt --otel-preset`).
//!
//! Every backend graphrt targets accepts OTLP/HTTP traces; what differs is the
//! endpoint path, the auth and routing headers, and sometimes a resource
//! attribute. A preset is a small static table of those, filled from each
//! vendor's own documented environment variables, so credentials never appear
//! on a command line (where `ps` would show them).
//!
//! | Preset | Base URL variable (default) | Traces path | Headers | Resource |
//! | --- | --- | --- | --- | --- |
//! | `langfuse` | `LANGFUSE_BASE_URL` / `LANGFUSE_HOST` (`https://cloud.langfuse.com`) | `/api/public/otel/v1/traces` | `Authorization: Basic base64(LANGFUSE_PUBLIC_KEY:LANGFUSE_SECRET_KEY)`, `x-langfuse-ingestion-version: 4` | – |
//! | `langsmith` | `LANGSMITH_ENDPOINT` (`https://api.smith.langchain.com`) | `/otel/v1/traces` | `x-api-key: LANGSMITH_API_KEY`, `Langsmith-Project: LANGSMITH_PROJECT`? | – |
//! | `braintrust` | `BRAINTRUST_API_URL` (`https://api.braintrust.dev`) | `/otel/v1/traces` | `Authorization: Bearer BRAINTRUST_API_KEY`, `x-bt-parent: BRAINTRUST_PARENT` | – |
//! | `opik` | `OPIK_URL_OVERRIDE` (`https://www.comet.com/opik/api`) | `/v1/private/otel/v1/traces` | `Authorization: OPIK_API_KEY`?, `Comet-Workspace: OPIK_WORKSPACE`?, `projectName: OPIK_PROJECT_NAME`? | `opik.project_name`? |
//! | `phoenix` | `PHOENIX_COLLECTOR_ENDPOINT` (`http://localhost:6006`) | `/v1/traces` | `Authorization: Bearer PHOENIX_API_KEY`? | `openinference.project.name` (`PHOENIX_PROJECT_NAME`)? |
//! | `mlflow` | `MLFLOW_TRACKING_URI` (`http://localhost:5000`) | `/v1/traces` | `x-mlflow-experiment-id: MLFLOW_EXPERIMENT_ID`, `x-mlflow-workspace: MLFLOW_WORKSPACE`?, `Authorization: Bearer MLFLOW_TRACKING_TOKEN`? | – |
//!
//! `?` marks optional values (left out when the variable is unset); the others
//! are required. None of these backends ingests OTLP metrics, so a preset turns
//! the metrics push off unless `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is set. An
//! explicit `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` still wins over the preset's.

use crate::config::ConfigError;

/// Where a header value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderSource {
    /// The variable's value as is.
    Env(&'static str),
    /// A fixed prefix (`Bearer `) followed by the variable's value.
    Prefixed(&'static str, &'static str),
    /// `Basic base64(<first>:<second>)`.
    Basic(&'static str, &'static str),
    /// A constant.
    Fixed(&'static str),
}

/// One header of a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderRule {
    pub name: &'static str,
    pub source: HeaderSource,
    /// Fail when a variable it needs is unset (otherwise the header is left out).
    pub required: bool,
}

/// One resource attribute of a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceRule {
    pub key: &'static str,
    pub env: &'static str,
}

/// An export profile of one vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    /// Variables naming the base URL, first set wins.
    pub base_env: &'static [&'static str],
    pub default_base: &'static str,
    /// Appended to the base URL (unless the base already ends with it).
    pub traces_path: &'static str,
    pub headers: &'static [HeaderRule],
    /// Optional resource attributes (set when the variable is).
    pub resource: &'static [ResourceRule],
}

const fn required(name: &'static str, source: HeaderSource) -> HeaderRule {
    HeaderRule { name, source, required: true }
}

const fn optional(name: &'static str, source: HeaderSource) -> HeaderRule {
    HeaderRule { name, source, required: false }
}

/// Every preset.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "langfuse",
        base_env: &["LANGFUSE_BASE_URL", "LANGFUSE_HOST"],
        default_base: "https://cloud.langfuse.com",
        traces_path: "/api/public/otel/v1/traces",
        headers: &[
            required("authorization", HeaderSource::Basic("LANGFUSE_PUBLIC_KEY", "LANGFUSE_SECRET_KEY")),
            // Real-time v4 ingestion; without it OTLP data can lag by up to 10 minutes.
            required("x-langfuse-ingestion-version", HeaderSource::Fixed("4")),
        ],
        resource: &[],
    },
    Preset {
        name: "langsmith",
        base_env: &["LANGSMITH_ENDPOINT"],
        default_base: "https://api.smith.langchain.com",
        traces_path: "/otel/v1/traces",
        headers: &[
            required("x-api-key", HeaderSource::Env("LANGSMITH_API_KEY")),
            optional("langsmith-project", HeaderSource::Env("LANGSMITH_PROJECT")),
        ],
        resource: &[],
    },
    Preset {
        name: "braintrust",
        base_env: &["BRAINTRUST_API_URL"],
        default_base: "https://api.braintrust.dev",
        traces_path: "/otel/v1/traces",
        headers: &[
            required("authorization", HeaderSource::Prefixed("Bearer ", "BRAINTRUST_API_KEY")),
            // `project_name:<name>`, `project_id:<id>` or `experiment_id:<id>`.
            required("x-bt-parent", HeaderSource::Env("BRAINTRUST_PARENT")),
        ],
        resource: &[],
    },
    Preset {
        name: "opik",
        base_env: &["OPIK_URL_OVERRIDE"],
        default_base: "https://www.comet.com/opik/api",
        traces_path: "/v1/private/otel/v1/traces",
        headers: &[
            // The API key itself, without `Bearer` (not needed by a local Opik).
            optional("authorization", HeaderSource::Env("OPIK_API_KEY")),
            optional("comet-workspace", HeaderSource::Env("OPIK_WORKSPACE")),
            optional("projectname", HeaderSource::Env("OPIK_PROJECT_NAME")),
        ],
        resource: &[ResourceRule { key: crate::semconv::OPIK_PROJECT_NAME, env: "OPIK_PROJECT_NAME" }],
    },
    Preset {
        name: "phoenix",
        base_env: &["PHOENIX_COLLECTOR_ENDPOINT"],
        default_base: "http://localhost:6006",
        traces_path: "/v1/traces",
        headers: &[optional("authorization", HeaderSource::Prefixed("Bearer ", "PHOENIX_API_KEY"))],
        resource: &[ResourceRule { key: crate::semconv::OPENINFERENCE_PROJECT_NAME, env: "PHOENIX_PROJECT_NAME" }],
    },
    Preset {
        name: "mlflow",
        base_env: &["MLFLOW_TRACKING_URI"],
        default_base: "http://localhost:5000",
        traces_path: "/v1/traces",
        headers: &[
            required("x-mlflow-experiment-id", HeaderSource::Env("MLFLOW_EXPERIMENT_ID")),
            optional("x-mlflow-workspace", HeaderSource::Env("MLFLOW_WORKSPACE")),
            optional("authorization", HeaderSource::Prefixed("Bearer ", "MLFLOW_TRACKING_TOKEN")),
        ],
        resource: &[],
    },
];

/// Whether a header carries a credential (an API key or token), as opposed to
/// routing data such as a project name.
pub fn is_credential_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
}

/// Environment variable selecting a preset.
pub const PRESET_ENV: &str = "GRAPHRT_OTEL_PRESET";

/// A preset resolved against the environment.
#[derive(Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The vendor's base URL, without the traces path.
    pub base_url: String,
    pub traces_endpoint: String,
    /// Values are secrets: never logged.
    pub headers: Vec<(String, String)>,
    pub resource: Vec<(String, String)>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("Resolved")
            .field("base_url", &graphrt_core::urls::redact_userinfo(&self.base_url))
            .field("traces_endpoint", &graphrt_core::urls::redact_userinfo(&self.traces_endpoint))
            .field("headers", &header_names)
            .field("resource", &self.resource)
            .finish()
    }
}

/// The preset named `name` (case-insensitive).
///
/// @throws ConfigError - When no preset has that name (the message lists them).
pub fn find(name: &str) -> Result<&'static Preset, ConfigError> {
    let name = name.trim();
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        let known: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        ConfigError::Invalid {
            var: PRESET_ENV.to_owned(),
            message: format!("unknown preset `{name}` (known: {})", known.join(", ")),
        }
    })
}

impl Preset {
    /// Resolve against `vars` (later entries win, empty values count as unset).
    ///
    /// @throws ConfigError - When a required variable is unset or the base URL is not http(s).
    pub fn resolve(&self, vars: &[(String, String)]) -> Result<Resolved, ConfigError> {
        let get = |k: &str| vars.iter().rev().find(|(n, v)| n == k && !v.trim().is_empty()).map(|(_, v)| v.trim());
        let base_url =
            self.base_env.iter().find_map(|var| get(var)).unwrap_or(self.default_base).trim_end_matches('/').to_owned();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(self.invalid(self.base_env.first().copied().unwrap_or(PRESET_ENV), "must be an http(s) URL"));
        }
        // A base URL may already name the traces endpoint; `base_url` is always
        // the base, so other APIs of the vendor (Langfuse scores) resolve from it.
        let base_url = base_url.strip_suffix(self.traces_path).unwrap_or(&base_url).to_owned();
        let traces_endpoint = format!("{base_url}{}", self.traces_path);
        let need = |var: &'static str, rule: &HeaderRule| -> Result<Option<&str>, ConfigError> {
            match get(var) {
                Some(value) => Ok(Some(value)),
                None if rule.required => Err(self.invalid(var, "is required by this preset")),
                None => Ok(None),
            }
        };
        let mut headers = Vec::with_capacity(self.headers.len());
        for rule in self.headers {
            let value = match rule.source {
                HeaderSource::Fixed(value) => Some(value.to_owned()),
                HeaderSource::Env(var) => need(var, rule)?.map(str::to_owned),
                HeaderSource::Prefixed(prefix, var) => need(var, rule)?.map(|v| format!("{prefix}{v}")),
                HeaderSource::Basic(user, password) => match (need(user, rule)?, need(password, rule)?) {
                    (Some(user), Some(password)) => Some(basic_auth(user, password)),
                    _ => None,
                },
            };
            if let Some(value) = value {
                headers.push((rule.name.to_owned(), value));
            }
        }
        let resource = self
            .resource
            .iter()
            .filter_map(|rule| get(rule.env).map(|v| (rule.key.to_owned(), v.to_owned())))
            .collect();
        Ok(Resolved { base_url, traces_endpoint, headers, resource })
    }

    fn invalid(&self, var: &str, message: &str) -> ConfigError {
        ConfigError::Invalid { var: var.to_owned(), message: format!("{message} (preset `{}`)", self.name) }
    }
}

/// `Basic base64(user:password)`.
pub fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
}

/// Standard base64 with padding.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |index: u32| char::from(ALPHABET.get((index & 63) as usize).copied().unwrap_or(b'A'));
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let triple = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        out.push(symbol(triple >> 18));
        out.push(symbol(triple >> 12));
        out.push(if chunk.len() > 1 { symbol(triple >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(triple) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn header<'a>(resolved: &'a Resolved, name: &str) -> Option<&'a str> {
        resolved.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, expected) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        assert_eq!(basic_auth("pk-lf-1", "sk-lf-2"), "Basic cGstbGYtMTpzay1sZi0y");
    }

    #[test]
    fn langfuse_uses_basic_auth_and_the_v4_header() {
        let preset = find("Langfuse").unwrap();
        let resolved =
            preset.resolve(&vars(&[("LANGFUSE_PUBLIC_KEY", "pk-lf-1"), ("LANGFUSE_SECRET_KEY", "sk-lf-2")])).unwrap();
        assert_eq!(resolved.traces_endpoint, "https://cloud.langfuse.com/api/public/otel/v1/traces");
        assert_eq!(header(&resolved, "authorization"), Some("Basic cGstbGYtMTpzay1sZi0y"));
        assert_eq!(header(&resolved, "x-langfuse-ingestion-version"), Some("4"));
        let shown = format!("{resolved:?}");
        assert!(!shown.contains("cGst"), "{shown}");
        let err = preset.resolve(&vars(&[("LANGFUSE_PUBLIC_KEY", "pk")])).unwrap_err().to_string();
        assert!(err.contains("LANGFUSE_SECRET_KEY") && err.contains("langfuse"), "{err}");
        let self_hosted = preset
            .resolve(&vars(&[
                ("LANGFUSE_HOST", "http://old:3000"),
                ("LANGFUSE_BASE_URL", "http://lf:3000/"),
                ("LANGFUSE_PUBLIC_KEY", "pk"),
                ("LANGFUSE_SECRET_KEY", "sk"),
            ]))
            .unwrap();
        assert_eq!(self_hosted.traces_endpoint, "http://lf:3000/api/public/otel/v1/traces");
        // A base URL that already names the traces endpoint still yields the base.
        let full = preset
            .resolve(&vars(&[
                ("LANGFUSE_BASE_URL", "https://lf.example/api/public/otel/v1/traces"),
                ("LANGFUSE_PUBLIC_KEY", "pk"),
                ("LANGFUSE_SECRET_KEY", "sk"),
            ]))
            .unwrap();
        assert_eq!(full.base_url, "https://lf.example");
        assert_eq!(full.traces_endpoint, "https://lf.example/api/public/otel/v1/traces");
    }

    #[test]
    fn every_vendor_fills_its_headers_and_attributes() {
        let langsmith = find("langsmith").unwrap().resolve(&vars(&[("LANGSMITH_API_KEY", "ls")])).unwrap();
        assert_eq!(langsmith.traces_endpoint, "https://api.smith.langchain.com/otel/v1/traces");
        assert_eq!(header(&langsmith, "x-api-key"), Some("ls"));
        assert_eq!(header(&langsmith, "langsmith-project"), None, "optional and unset");

        let braintrust = find("braintrust")
            .unwrap()
            .resolve(&vars(&[("BRAINTRUST_API_KEY", "bt"), ("BRAINTRUST_PARENT", "experiment_id:e1")]))
            .unwrap();
        assert_eq!(header(&braintrust, "authorization"), Some("Bearer bt"));
        assert_eq!(header(&braintrust, "x-bt-parent"), Some("experiment_id:e1"));
        assert!(find("braintrust").unwrap().resolve(&vars(&[("BRAINTRUST_API_KEY", "bt")])).is_err());

        let opik = find("opik")
            .unwrap()
            .resolve(&vars(&[("OPIK_API_KEY", "op"), ("OPIK_PROJECT_NAME", "agents"), ("OPIK_WORKSPACE", "ws")]))
            .unwrap();
        assert_eq!(opik.traces_endpoint, "https://www.comet.com/opik/api/v1/private/otel/v1/traces");
        assert_eq!(header(&opik, "authorization"), Some("op"), "no Bearer prefix");
        assert_eq!(header(&opik, "comet-workspace"), Some("ws"));
        assert_eq!(opik.resource, vec![("opik.project_name".to_owned(), "agents".to_owned())]);

        let phoenix = find("phoenix").unwrap().resolve(&vars(&[("PHOENIX_PROJECT_NAME", "p")])).unwrap();
        assert_eq!(phoenix.traces_endpoint, "http://localhost:6006/v1/traces");
        assert!(phoenix.headers.is_empty());
        assert_eq!(phoenix.resource, vec![("openinference.project.name".to_owned(), "p".to_owned())]);
        let phoenix =
            find("phoenix").unwrap().resolve(&vars(&[("PHOENIX_COLLECTOR_ENDPOINT", "http://px:6006/v1/traces")]));
        assert_eq!(phoenix.unwrap().traces_endpoint, "http://px:6006/v1/traces", "path not doubled");

        let mlflow = find("mlflow").unwrap();
        assert!(mlflow.resolve(&[]).unwrap_err().to_string().contains("MLFLOW_EXPERIMENT_ID"));
        let mlflow = mlflow.resolve(&vars(&[("MLFLOW_EXPERIMENT_ID", "7")])).unwrap();
        assert_eq!(mlflow.traces_endpoint, "http://localhost:5000/v1/traces");
        assert_eq!(header(&mlflow, "x-mlflow-experiment-id"), Some("7"));
    }

    #[test]
    fn unknown_presets_and_bad_urls_are_errors() {
        let err = find("datadog").unwrap_err().to_string();
        assert!(err.contains("langfuse") && err.contains("mlflow"), "{err}");
        let bad = find("phoenix").unwrap().resolve(&vars(&[("PHOENIX_COLLECTOR_ENDPOINT", "px:6006")]));
        assert!(bad.is_err());
    }
}
