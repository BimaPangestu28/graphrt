//! OTLP/JSON encoding (opentelemetry-proto JSON mapping: hex ids, 64-bit integers
//! as strings, lowerCamelCase field names).

use serde_json::{Value as Json, json};

use crate::span::{AttrValue, Attributes, Span};

/// Instrumentation scope name used for every span and metric.
pub const SCOPE_NAME: &str = "graphrt";

/// Resource attributes describing the process emitting telemetry.
#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    pub attributes: Attributes,
}

impl Resource {
    /// Resource with `service.name` and the SDK attributes.
    pub fn new(service_name: &str) -> Self {
        Self {
            attributes: vec![
                ("service.name".to_owned(), AttrValue::from(service_name)),
                ("telemetry.sdk.name".to_owned(), AttrValue::from("graphrt-otel")),
                ("telemetry.sdk.language".to_owned(), AttrValue::from("rust")),
                ("telemetry.sdk.version".to_owned(), AttrValue::from(env!("CARGO_PKG_VERSION"))),
            ],
        }
    }

    /// Add or replace an attribute.
    pub fn with(mut self, key: &str, value: impl Into<AttrValue>) -> Self {
        let value = value.into();
        match self.attributes.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((key.to_owned(), value)),
        }
        self
    }
}

impl Default for Resource {
    fn default() -> Self {
        Self::new("graphrt")
    }
}

/// Encode one attribute value.
pub fn any_value(value: &AttrValue) -> Json {
    match value {
        AttrValue::Str(s) => json!({ "stringValue": s }),
        AttrValue::Int(i) => json!({ "intValue": i.to_string() }),
        AttrValue::Double(d) if d.is_finite() => json!({ "doubleValue": d }),
        AttrValue::Double(d) => json!({ "stringValue": d.to_string() }),
        AttrValue::Bool(b) => json!({ "boolValue": b }),
        AttrValue::Structured(value) => structured_value(value),
    }
}

/// Encode a JSON value as an OTLP `AnyValue` (objects become `kvlistValue`).
fn structured_value(value: &Json) -> Json {
    match value {
        Json::Null => json!({}),
        Json::Bool(b) => json!({ "boolValue": b }),
        Json::Number(n) => match n.as_i64() {
            Some(i) => json!({ "intValue": i.to_string() }),
            None => any_value(&AttrValue::Double(n.as_f64().unwrap_or(f64::NAN))),
        },
        Json::String(s) => json!({ "stringValue": s }),
        Json::Array(items) => {
            json!({ "arrayValue": { "values": items.iter().map(structured_value).collect::<Vec<_>>() } })
        }
        Json::Object(map) => {
            let values: Vec<Json> =
                map.iter().map(|(k, v)| json!({ "key": k, "value": structured_value(v) })).collect();
            json!({ "kvlistValue": { "values": values } })
        }
    }
}

/// Encode an attribute list as OTLP `KeyValue`s.
pub fn attributes(attrs: &Attributes) -> Json {
    Json::Array(attrs.iter().map(|(k, v)| json!({ "key": k, "value": any_value(v) })).collect())
}

/// Encode one span.
pub fn span(span: &Span) -> Json {
    let mut out = json!({
        "traceId": span.trace_id.to_hex(),
        "spanId": span.span_id.to_hex(),
        "name": span.name,
        "kind": span.kind.code(),
        "startTimeUnixNano": span.start_unix_nano.to_string(),
        "endTimeUnixNano": span.end_unix_nano.max(span.start_unix_nano).to_string(),
        "attributes": attributes(&span.attributes),
        "events": span.events.iter().map(|e| json!({
            "timeUnixNano": e.time_unix_nano.to_string(),
            "name": e.name,
            "attributes": attributes(&e.attributes),
        })).collect::<Vec<_>>(),
        "status": { "code": span.status.code(), "message": span.status_message },
    });
    if let Some(obj) = out.as_object_mut() {
        if let Some(parent) = span.parent_span_id {
            obj.insert("parentSpanId".into(), Json::String(parent.to_hex()));
        }
        if span.dropped_events > 0 {
            obj.insert("droppedEventsCount".into(), json!(span.dropped_events));
        }
    }
    out
}

/// Encode an `ExportTraceServiceRequest`.
pub fn traces_request(resource: &Resource, spans: &[Span]) -> Json {
    json!({
        "resourceSpans": [{
            "resource": { "attributes": attributes(&resource.attributes) },
            "scopeSpans": [{
                "scope": { "name": SCOPE_NAME, "version": env!("CARGO_PKG_VERSION") },
                "spans": spans.iter().map(span).collect::<Vec<_>>(),
            }],
        }],
    })
}

/// Wrap encoded metrics in an `ExportMetricsServiceRequest`.
pub fn metrics_request(resource: &Resource, metrics: Vec<Json>) -> Json {
    json!({
        "resourceMetrics": [{
            "resource": { "attributes": attributes(&resource.attributes) },
            "scopeMetrics": [{
                "scope": { "name": SCOPE_NAME, "version": env!("CARGO_PKG_VERSION") },
                "metrics": metrics,
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{SpanId, StatusCode, TraceId};

    #[test]
    fn span_encoding_uses_spec_field_names() {
        let mut s = Span::new(TraceId::random(), Some(SpanId::random()), "node", 10);
        s.end_unix_nano = 20;
        s.set("graphrt.node.id", "llm");
        s.set("gen_ai.usage.input_tokens", 12u64);
        s.set("ok", true);
        s.set("ratio", 0.5);
        s.add_event("retry", 15, vec![("attempt".into(), AttrValue::from(2u32))]);
        s.error("boom");
        let j = span(&s);
        assert_eq!(j["traceId"].as_str().map(str::len), Some(32));
        assert_eq!(j["spanId"].as_str().map(str::len), Some(16));
        assert!(j["parentSpanId"].is_string());
        assert_eq!(j["startTimeUnixNano"], "10");
        assert_eq!(j["endTimeUnixNano"], "20");
        assert_eq!(j["kind"], 1);
        assert_eq!(j["status"]["code"], StatusCode::Error.code());
        assert_eq!(j["attributes"][1]["value"]["intValue"], "12");
        assert_eq!(j["attributes"][2]["value"]["boolValue"], true);
        assert_eq!(j["attributes"][3]["value"]["doubleValue"], 0.5);
        assert_eq!(j["events"][0]["timeUnixNano"], "15");
        let structured = any_value(&AttrValue::Structured(json!([{"role": "user", "n": 2, "ok": true}])));
        let item = &structured["arrayValue"]["values"][0]["kvlistValue"]["values"];
        assert_eq!(item[0], json!({"key": "role", "value": {"stringValue": "user"}}));
        assert_eq!(item[1]["value"]["intValue"], "2");
        assert_eq!(item[2]["value"]["boolValue"], true);
        let req = traces_request(&Resource::new("svc"), &[s]);
        assert_eq!(req["resourceSpans"][0]["scopeSpans"][0]["scope"]["name"], SCOPE_NAME);
        assert_eq!(req["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"], "svc");
    }
}
