//! Cumulative metrics: run and node counters, node duration histograms and token
//! usage, encoded as OTLP metrics.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value as Json, json};

use crate::otlp;
use crate::span::{AttrValue, now_unix_nanos};

/// Histogram bucket bounds for durations, in milliseconds.
pub const DURATION_BOUNDS_MS: &[f64] =
    &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0];

#[derive(Debug, Default, Clone)]
struct Histogram {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    buckets: Vec<u64>,
}

impl Histogram {
    fn record(&mut self, value: f64) {
        if self.buckets.is_empty() {
            self.buckets = vec![0; DURATION_BOUNDS_MS.len() + 1];
            self.min = value;
            self.max = value;
        }
        self.count += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        let idx = DURATION_BOUNDS_MS.iter().position(|b| value <= *b).unwrap_or(DURATION_BOUNDS_MS.len());
        if let Some(slot) = self.buckets.get_mut(idx) {
            *slot += 1;
        }
    }
}

type Key = Vec<(String, String)>;

#[derive(Debug, Default)]
struct Inner {
    runs: BTreeMap<Key, u64>,
    nodes: BTreeMap<Key, u64>,
    tokens: BTreeMap<Key, u64>,
    node_duration: BTreeMap<Key, Histogram>,
    run_duration: BTreeMap<Key, Histogram>,
}

/// Process-wide cumulative metrics. Share one instance (`Arc<Metrics>`) between all
/// tracers of a process.
#[derive(Debug)]
pub struct Metrics {
    start_unix_nano: u64,
    inner: Mutex<Inner>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

fn key(pairs: &[(&str, &str)]) -> Key {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

impl Metrics {
    pub fn new() -> Self {
        Self { start_unix_nano: now_unix_nanos(), inner: Mutex::new(Inner::default()) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    /// Count a finished run and its duration.
    pub fn record_run(&self, workflow: &str, status: &str, duration_ms: u64) {
        let k = key(&[("graphrt.workflow.name", workflow), ("graphrt.run.status", status)]);
        #[allow(clippy::cast_precision_loss)]
        let ms = duration_ms as f64;
        self.with(|m| {
            *m.runs.entry(k.clone()).or_default() += 1;
            m.run_duration.entry(k).or_default().record(ms);
        });
    }

    /// Count a finished node execution and its duration.
    pub fn record_node(&self, kind: &str, status: &str, duration_ms: u64) {
        let k = key(&[("graphrt.node.kind", kind), ("graphrt.node.status", status)]);
        #[allow(clippy::cast_precision_loss)]
        let ms = duration_ms as f64;
        self.with(|m| {
            *m.nodes.entry(k.clone()).or_default() += 1;
            m.node_duration.entry(k).or_default().record(ms);
        });
    }

    /// Add model token usage.
    pub fn record_tokens(&self, model: &str, input: u64, output: u64) {
        self.with(|m| {
            for (kind, n) in [("input", input), ("output", output)] {
                if n > 0 {
                    let k = key(&[("gen_ai.request.model", model), ("gen_ai.token.type", kind)]);
                    *m.tokens.entry(k).or_default() += n;
                }
            }
        });
    }

    /// Total runs recorded (all statuses), for tests and diagnostics.
    pub fn total_runs(&self) -> u64 {
        self.with(|m| m.runs.values().sum())
    }

    /// Encode the current cumulative values as OTLP metric objects.
    pub fn encode(&self) -> Vec<Json> {
        let now = now_unix_nanos().to_string();
        let start = self.start_unix_nano.to_string();
        let attrs = |k: &Key| {
            let list: Vec<(String, AttrValue)> =
                k.iter().map(|(a, b)| (a.clone(), AttrValue::from(b.as_str()))).collect();
            otlp::attributes(&list)
        };
        let sum = |name: &str, unit: &str, desc: &str, data: &BTreeMap<Key, u64>| {
            json!({
                "name": name, "unit": unit, "description": desc,
                "sum": {
                    "aggregationTemporality": 2,
                    "isMonotonic": true,
                    "dataPoints": data.iter().map(|(k, v)| json!({
                        "attributes": attrs(k), "startTimeUnixNano": start, "timeUnixNano": now, "asInt": v.to_string(),
                    })).collect::<Vec<_>>(),
                },
            })
        };
        let histogram = |name: &str, desc: &str, data: &BTreeMap<Key, Histogram>| {
            json!({
                "name": name, "unit": "ms", "description": desc,
                "histogram": {
                    "aggregationTemporality": 2,
                    "dataPoints": data.iter().map(|(k, h)| json!({
                        "attributes": attrs(k), "startTimeUnixNano": start, "timeUnixNano": now,
                        "count": h.count.to_string(), "sum": h.sum, "min": h.min, "max": h.max,
                        "bucketCounts": h.buckets.iter().map(u64::to_string).collect::<Vec<_>>(),
                        "explicitBounds": DURATION_BOUNDS_MS,
                    })).collect::<Vec<_>>(),
                },
            })
        };
        self.with(|m| {
            let mut out = Vec::new();
            if !m.runs.is_empty() {
                out.push(sum("graphrt.runs", "{run}", "Finished workflow runs.", &m.runs));
                out.push(histogram("graphrt.run.duration", "Workflow run duration.", &m.run_duration));
            }
            if !m.nodes.is_empty() {
                out.push(sum("graphrt.nodes", "{execution}", "Finished node executions.", &m.nodes));
                out.push(histogram("graphrt.node.duration", "Node execution duration.", &m.node_duration));
            }
            if !m.tokens.is_empty() {
                out.push(sum("graphrt.tokens", "{token}", "Model tokens used.", &m.tokens));
            }
            out
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_histograms_encode() {
        let m = Metrics::new();
        m.record_run("wf", "succeeded", 120);
        m.record_run("wf", "succeeded", 3);
        m.record_node("llm", "succeeded", 900);
        m.record_tokens("gpt", 10, 5);
        assert_eq!(m.total_runs(), 2);
        let enc = m.encode();
        let runs = enc.iter().find(|j| j["name"] == "graphrt.runs").unwrap();
        assert_eq!(runs["sum"]["dataPoints"][0]["asInt"], "2");
        assert_eq!(runs["sum"]["aggregationTemporality"], 2);
        let dur = enc.iter().find(|j| j["name"] == "graphrt.run.duration").unwrap();
        let point = &dur["histogram"]["dataPoints"][0];
        assert_eq!(point["count"], "2");
        assert_eq!(point["bucketCounts"].as_array().map(Vec::len), Some(DURATION_BOUNDS_MS.len() + 1));
        let tokens = enc.iter().find(|j| j["name"] == "graphrt.tokens").unwrap();
        assert_eq!(tokens["sum"]["dataPoints"].as_array().map(Vec::len), Some(2));
    }
}
