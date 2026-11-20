//! Cumulative metrics: run and node counters, node duration histograms, token
//! usage and evaluation scores, encoded as OTLP metrics.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value as Json, json};

use crate::otlp;
use crate::span::{AttrValue, now_unix_nanos};

/// Histogram bucket bounds for durations, in milliseconds.
pub const DURATION_BOUNDS_MS: &[f64] =
    &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0];

/// Histogram bucket bounds for evaluation scores (`0..=1`).
pub const SCORE_BOUNDS: &[f64] = &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

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
        self.record_in(value, DURATION_BOUNDS_MS);
    }

    fn record_in(&mut self, value: f64, bounds: &[f64]) {
        if self.buckets.is_empty() {
            self.buckets = vec![0; bounds.len() + 1];
            self.min = value;
            self.max = value;
        }
        self.count += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        let idx = bounds.iter().position(|b| value <= *b).unwrap_or(bounds.len());
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
    scores: BTreeMap<Key, Histogram>,
    eval_dropped: BTreeMap<Key, u64>,
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

    /// Record an evaluation score (`graphrt.eval.score`) of evaluator `name`
    /// on a run of `workflow`. Non-finite values are ignored.
    pub fn record_score(&self, name: &str, workflow: &str, source: &str, score: f64) {
        if !score.is_finite() {
            return;
        }
        let k = key(&[
            ("gen_ai.evaluation.name", name),
            ("graphrt.workflow.name", workflow),
            ("graphrt.evaluation.source", source),
        ]);
        self.with(|m| m.scores.entry(k).or_default().record_in(score, SCORE_BOUNDS));
    }

    /// Count a run whose online evaluation was dropped because the queue was full.
    pub fn record_eval_dropped(&self, workflow: &str) {
        let k = key(&[("graphrt.workflow.name", workflow)]);
        self.with(|m| *m.eval_dropped.entry(k).or_default() += 1);
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
        let histogram = |name: &str, unit: &str, desc: &str, bounds: &[f64], data: &BTreeMap<Key, Histogram>| {
            json!({
                "name": name, "unit": unit, "description": desc,
                "histogram": {
                    "aggregationTemporality": 2,
                    "dataPoints": data.iter().map(|(k, h)| json!({
                        "attributes": attrs(k), "startTimeUnixNano": start, "timeUnixNano": now,
                        "count": h.count.to_string(), "sum": h.sum, "min": h.min, "max": h.max,
                        "bucketCounts": h.buckets.iter().map(u64::to_string).collect::<Vec<_>>(),
                        "explicitBounds": bounds,
                    })).collect::<Vec<_>>(),
                },
            })
        };
        self.with(|m| {
            let mut out = Vec::new();
            if !m.runs.is_empty() {
                out.push(sum("graphrt.runs", "{run}", "Finished workflow runs.", &m.runs));
                out.push(histogram(
                    "graphrt.run.duration",
                    "ms",
                    "Workflow run duration.",
                    DURATION_BOUNDS_MS,
                    &m.run_duration,
                ));
            }
            if !m.nodes.is_empty() {
                out.push(sum("graphrt.nodes", "{execution}", "Finished node executions.", &m.nodes));
                out.push(histogram(
                    "graphrt.node.duration",
                    "ms",
                    "Node execution duration.",
                    DURATION_BOUNDS_MS,
                    &m.node_duration,
                ));
            }
            if !m.tokens.is_empty() {
                out.push(sum("graphrt.tokens", "{token}", "Model tokens used.", &m.tokens));
            }
            if !m.eval_dropped.is_empty() {
                out.push(sum(
                    crate::semconv::graphrt::EVAL_DROPPED_METRIC,
                    "{run}",
                    "Sampled runs not evaluated because the online evaluation queue was full.",
                    &m.eval_dropped,
                ));
            }
            if !m.scores.is_empty() {
                out.push(histogram(
                    crate::semconv::graphrt::EVAL_SCORE_METRIC,
                    "1",
                    "Evaluation scores (0..1) of runs.",
                    SCORE_BOUNDS,
                    &m.scores,
                ));
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
        assert!(!enc.iter().any(|j| j["name"] == "graphrt.eval.score"), "no scores, no metric");
    }

    #[test]
    fn scores_are_a_histogram_by_evaluator() {
        let m = Metrics::new();
        m.record_score("helpful", "chat", "online", 0.25);
        m.record_score("helpful", "chat", "online", 1.0);
        m.record_score("helpful", "chat", "online", f64::NAN);
        m.record_score("tone", "chat", "human", 0.0);
        let enc = m.encode();
        let scores = enc.iter().find(|j| j["name"] == "graphrt.eval.score").unwrap();
        assert_eq!(scores["unit"], "1");
        let points = scores["histogram"]["dataPoints"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        let helpful = points.iter().find(|p| p["attributes"].to_string().contains("helpful")).unwrap();
        assert_eq!(helpful["count"], "2");
        assert_eq!(helpful["sum"], 1.25);
        assert_eq!(helpful["explicitBounds"].as_array().map(Vec::len), Some(SCORE_BOUNDS.len()));
        m.record_eval_dropped("chat");
        m.record_eval_dropped("chat");
        let enc = m.encode();
        let dropped = enc.iter().find(|j| j["name"] == "graphrt.eval.dropped").unwrap();
        assert_eq!(dropped["sum"]["dataPoints"][0]["asInt"], "2");
    }
}
