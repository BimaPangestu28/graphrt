//! Retrieval metrics over the hits of `knowledge-retrieval` nodes, against
//! labelled document (or chunk) ids: recall@k, precision@k and MRR.
//!
//! `value` lists the relevant ids (a hit is relevant when its `document_id` or
//! chunk `id` is listed); `k` limits the ranking (default: every hit); `node`
//! restricts to one retrieval node. Without a `threshold`, recall must be 1
//! (every relevant document retrieved) and precision / MRR must be above 0
//! (at least one relevant hit in the top k).

use serde_json::Value as Json;

use super::{AssertionRegistry, Outcome, only_keys, sync_assertion};
use crate::capture::RetrievalHit;
use crate::config::AssertionSpec;
use crate::error::EvalError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Metric {
    Recall,
    Precision,
    Mrr,
}

pub(super) fn register(registry: &mut AssertionRegistry) {
    registry.register("retrieval:recall-at-k", |spec| retrieval(spec, Metric::Recall));
    registry.register("retrieval:precision-at-k", |spec| retrieval(spec, Metric::Precision));
    registry.register("retrieval:mrr", |spec| retrieval(spec, Metric::Mrr));
}

fn score(metric: Metric, ranked: &[RetrievalHit], relevant: &[String]) -> f64 {
    let is_relevant = |hit: &RetrievalHit| relevant.iter().any(|label| hit.is(label));
    match metric {
        Metric::Recall => {
            let found = relevant.iter().filter(|label| ranked.iter().any(|hit| hit.is(label))).count();
            found as f64 / relevant.len() as f64
        }
        Metric::Precision if ranked.is_empty() => 0.0,
        Metric::Precision => ranked.iter().filter(|hit| is_relevant(hit)).count() as f64 / ranked.len() as f64,
        Metric::Mrr => ranked.iter().position(is_relevant).map_or(0.0, |rank| 1.0 / (rank as f64 + 1.0)),
    }
}

fn retrieval(spec: &AssertionSpec, metric: Metric) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &["k", "node"])?;
    let relevant: Vec<String> = match spec.value.as_ref() {
        Some(Json::String(s)) => vec![s.clone()],
        Some(Json::Array(items)) => items
            .iter()
            .map(|v| match v {
                Json::String(s) => Ok(s.clone()),
                Json::Number(n) => Ok(n.to_string()),
                _ => Err(EvalError::invalid("`value` must list document ids")),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(EvalError::invalid("`value` (the relevant document ids) is required")),
    };
    if relevant.is_empty() {
        return Err(EvalError::invalid("`value` must list at least one relevant document id"));
    }
    let k = match spec.config.get("k") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|k| usize::try_from(k).ok())
                .filter(|k| *k > 0)
                .ok_or_else(|| EvalError::invalid("`k` must be a positive integer"))?,
        ),
    };
    let node = match spec.config.get("node") {
        None => None,
        Some(Json::String(s)) => Some(s.clone()),
        Some(_) => return Err(EvalError::invalid("`node` must be a node id")),
    };
    let threshold = spec.threshold;
    if threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
        return Err(EvalError::invalid("`threshold` must be between 0 and 1"));
    }
    let name = match metric {
        Metric::Recall => "recall",
        Metric::Precision => "precision",
        Metric::Mrr => "MRR",
    };
    Ok(sync_assertion("retrieval", move |ctx| {
        let mut ranked: Vec<RetrievalHit> = ctx.capture.retrieval_hits();
        if let Some(node) = &node {
            ranked.retain(|hit| &hit.node_id == node);
        }
        if let Some(k) = k {
            ranked.truncate(k);
        }
        let value = score(metric, &ranked, &relevant);
        let pass = match (threshold, metric) {
            (Some(t), _) => value + 1e-9 >= t,
            (None, Metric::Recall) => value + 1e-9 >= 1.0,
            (None, _) => value > 0.0,
        };
        let at = k.map(|k| format!("@{k}")).unwrap_or_default();
        let retrieved: Vec<&str> = ranked
            .iter()
            .map(|h| if h.document_id.is_empty() { h.id.as_str() } else { h.document_id.as_str() })
            .collect();
        Outcome::graded(
            pass,
            value,
            format!(
                "{name}{at} = {value:.3} (relevant: {}; retrieved: [{}])",
                relevant.join(", "),
                retrieved.join(", ")
            ),
        )
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use graphrt_core::{Event, Value};
    use serde_json::json;

    use super::super::test_support::*;
    use crate::capture::RunCapture;

    fn retrieval_run() -> RunCapture {
        let mut outputs = graphrt_core::variables::Variables::new();
        outputs.insert(
            "result",
            Value::from_json(json!([
                {"id": "c7", "document_id": "faq", "score": 0.9, "content": "..."},
                {"id": "c1", "document_id": "refund-policy", "score": 0.8, "content": "..."},
                {"id": "c2", "document_id": "refund-policy", "score": 0.7, "content": "..."},
                {"id": "c9", "document_id": "shipping", "score": 0.5, "content": "..."}
            ])),
        );
        let events = vec![
            Event::NodeStarted {
                frame: Arc::from(""),
                node_id: Arc::from("retrieve"),
                exec_id: Arc::from("retrieve#1"),
                kind: Arc::from("knowledge-retrieval"),
                title: Arc::from("Retrieve"),
                attempt: 1,
            },
            Event::NodeSucceeded {
                frame: Arc::from(""),
                node_id: Arc::from("retrieve"),
                exec_id: Arc::from("retrieve#1"),
                outputs,
                branch: None,
                usage: None,
                metadata: Default::default(),
                elapsed_ms: 1,
            },
        ];
        RunCapture::from_events(events, String::new(), 1)
    }

    #[test]
    fn recall_precision_and_mrr() {
        let run = retrieval_run();
        let score = |spec| eval(spec, &run);
        let recall = score(json!({"type": "retrieval:recall-at-k", "value": ["refund-policy", "shipping"], "k": 3}));
        assert!(!recall.pass && recall.score == 0.5, "{recall:?}");
        let recall = score(json!({"type": "retrieval:recall-at-k", "value": ["refund-policy", "shipping"]}));
        assert!(recall.pass && recall.score == 1.0);
        let recall = score(
            json!({"type": "retrieval:recall-at-k", "value": ["refund-policy", "shipping"], "k": 3, "threshold": 0.5}),
        );
        assert!(recall.pass);
        assert!(score(json!({"type": "retrieval:recall-at-k", "value": "c9"})).pass, "chunk ids work too");

        let precision = score(json!({"type": "retrieval:precision-at-k", "value": ["refund-policy"], "k": 2}));
        assert!(precision.pass && precision.score == 0.5, "{precision:?}");
        let precision = score(json!({"type": "retrieval:precision-at-k", "value": ["refund-policy"], "k": 1}));
        assert!(!precision.pass && precision.score == 0.0);

        let mrr = score(json!({"type": "retrieval:mrr", "value": ["refund-policy"]}));
        assert!(mrr.pass && mrr.score == 0.5, "{mrr:?}");
        let mrr = score(json!({"type": "retrieval:mrr", "value": ["refund-policy"], "threshold": 0.9}));
        assert!(!mrr.pass);
        assert!(!score(json!({"type": "retrieval:mrr", "value": ["refund-policy"], "node": "other"})).pass);
        assert!(try_eval(json!({"type": "retrieval:mrr", "value": []}), &run).is_err());
        assert!(try_eval(json!({"type": "retrieval:mrr", "value": ["a"], "k": 0}), &run).is_err());
    }
}
