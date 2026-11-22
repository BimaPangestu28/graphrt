//! # graphrt-http
//!
//! Outbound HTTP for workflows:
//!
//! - [`HttpClient`]: a shared client service enforcing an [`EgressPolicy`]
//!   (SSRF protection, host allow/deny lists, redirect re-validation, response
//!   size caps, no environment proxies).
//! - [`HttpRequestNode`]: the `http-request` node.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use graphrt_core::{Registry, Services};
//! use graphrt_http::{EgressPolicy, HttpClient};
//!
//! let mut registry = Registry::with_builtins();
//! graphrt_http::register(&mut registry);
//! let services = Services::new().with(HttpClient::new(EgressPolicy::default())?);
//! # let _ = (registry, services);
//! # Ok(())
//! # }
//! ```

mod body;
mod client;
mod node;
mod policy;

mod files;
pub use client::{ClientError, HttpClient};
pub use files::HttpFileResolver;
pub use node::{HttpRequestNode, INLINE_FILE_LIMIT, KIND};
pub use policy::{EgressError, EgressPolicy, is_public};

use std::sync::Arc;

use graphrt_core::node::{Node, Registry};

/// Register the `http-request` node kind.
pub fn register(registry: &mut Registry) {
    registry.register_lazily_described(KIND, descriptor, |doc| {
        Ok(Arc::new(HttpRequestNode::from_doc(doc)?) as Arc<dyn Node>)
    });
}

/// Catalog descriptor of the `http-request` node.
pub fn descriptor() -> graphrt_core::catalog::NodeDescriptor {
    use graphrt_core::ValueType;
    use graphrt_core::catalog::schema::{nested, object, selector, template};
    use graphrt_core::catalog::{NodeCategory, NodeDescriptor, OutputDescriptor};
    use serde_json::json;

    let kv_item = nested(json!({ "key": template("Name"), "value": template("Value") }), &["key"]);
    let kv = json!({
        "description": "`k: v` lines, a list of {key, value}, or a map; keys and values are templates",
        "oneOf": [
            { "type": "string" },
            { "type": "array", "items": kv_item },
            { "type": "object", "additionalProperties": { "type": "string" } }
        ]
    });
    let body_item = nested(
        json!({
            "key": template("Field name"),
            "type": { "type": "string", "enum": ["text", "file"] },
            "value": template("Field value"),
            "file": selector()
        }),
        &[],
    );
    NodeDescriptor::new(KIND, "HTTP Request", NodeCategory::Integration)
        .describe(
            "Sends an HTTP request through the egress policy (private networks blocked by default, \
             redirects re-checked, credentials stripped on cross-origin redirects). Non-2xx responses \
             fail the node unless `fail_on_error_status` is false.",
        )
        .icon("globe")
        .source("graphrt-http")
        .schema(object(
            json!({
                "method": { "type": "string", "default": "get", "description": "HTTP method (case-insensitive)" },
                "url": template("Request URL"),
                "headers": kv,
                "params": kv,
                "authorization": nested(json!({
                    "type": { "type": "string", "enum": ["no-auth", "api-key"], "default": "no-auth" },
                    "config": nested(json!({
                        "type": { "type": "string", "enum": ["bearer", "basic", "custom"], "default": "bearer" },
                        "api_key": template("Credential; usually {{#env.NAME#}}"),
                        "header": { "type": "string", "description": "Header name for `custom`" }
                    }), &[])
                }), &[]),
                "body": nested(json!({
                    "type": { "type": "string", "enum": ["none", "json", "raw-text", "form-data", "x-www-form-urlencoded", "binary"], "default": "none" },
                    "data": { "oneOf": [ { "type": "string" }, { "type": "array", "items": body_item } ] }
                }), &[]),
                "timeout": nested(json!({
                    "connect": { "type": "number", "minimum": 0 },
                    "read": { "type": "number", "minimum": 0 },
                    "write": { "type": "number", "minimum": 0 },
                    "max_connect_timeout": { "type": "number", "minimum": 0 },
                    "max_read_timeout": { "type": "number", "minimum": 0 },
                    "max_write_timeout": { "type": "number", "minimum": 0 }
                }), &[]),
                "ssl_verify": { "type": "boolean", "default": true },
                "fail_on_error_status": { "type": "boolean", "default": true },
                "max_response_bytes": { "type": "integer", "minimum": 0 },
                "retry_non_idempotent": { "type": "boolean", "default": false },
                "idempotency_key": { "type": "boolean", "default": true },
                "parse_json": { "type": "string", "enum": ["lenient", "strict"], "default": "lenient" }
            }),
            &["url"],
        ))
        .output(OutputDescriptor::new("status_code", ValueType::Integer))
        .output(OutputDescriptor::new("body", ValueType::String).describe("Response text (empty for binary bodies)"))
        .output(OutputDescriptor::new("json", ValueType::Any).describe("Parsed body for JSON responses"))
        .output(OutputDescriptor::new("headers", ValueType::Object))
        .output(OutputDescriptor::new("files", ValueType::ArrayFile).describe("Binary response as a file"))
}
