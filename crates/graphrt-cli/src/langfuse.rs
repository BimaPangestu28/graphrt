//! Langfuse score upload (`graphrt serve --langfuse-scores`, cargo feature
//! `langfuse`).
//!
//! Langfuse does not read scores from OTLP, so each stored score (online or
//! human) is also POSTed to `<LANGFUSE_BASE_URL>/api/public/scores`, attached
//! to the run's trace and root observation (`traceId` = the run's OTel trace
//! id, `observationId` = its root span id). The score's deterministic `id` is
//! the idempotency key, so a retried or repeated upload overwrites instead of
//! duplicating, and Langfuse accepts a score before its trace arrives.
//!
//! Credentials are those of the `langfuse` OTel preset: `LANGFUSE_PUBLIC_KEY`,
//! `LANGFUSE_SECRET_KEY` and `LANGFUSE_BASE_URL` (or `LANGFUSE_HOST`) from the
//! environment or `--env-file`. Requests go through the server's egress policy
//! (`graphrt-http`): a private or loopback Langfuse needs
//! `--allow-private-network`, and redirects are not followed. Uploads run on
//! one background task from a bounded queue; a full queue drops the upload
//! (counted), and 408 / 429 / 5xx / network errors are retried with backoff.
//! Runs without a trace id (not traced) are skipped and counted.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use graphrt_core::store::RunRecord;
use graphrt_core::urls::redact_userinfo;
use graphrt_http::{EgressPolicy, HttpClient};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

use crate::error::CliError;
use crate::scores::{Score, ScoreValue};

/// Uploads waiting; more are dropped.
const QUEUE: usize = 1024;
const MAX_ATTEMPTS: u32 = 4;
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const TIMEOUT: Duration = Duration::from_secs(10);
/// Langfuse's limit on comments is generous; keep uploads small anyway.
const MAX_COMMENT_CHARS: usize = 2000;

#[derive(Debug, Default)]
struct Counters {
    sent: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    untraced: AtomicU64,
}

/// Where and how scores are sent.
struct Target {
    url: reqwest::Url,
    authorization: String,
    client: HttpClient,
    backoff: Duration,
}

/// Sends scores to Langfuse in the background.
#[derive(Debug)]
pub struct LangfuseUploader {
    tx: mpsc::Sender<Json>,
    counters: Arc<Counters>,
    url: String,
}

impl LangfuseUploader {
    /// Configure from the `langfuse` preset variables in `vars` and start the
    /// upload task (on the current tokio runtime).
    ///
    /// @throws CliError (usage) - When a Langfuse variable is missing or the URL
    ///   is refused by the egress policy.
    pub fn start(vars: &[(String, String)], allow_private: bool) -> Result<Self, CliError> {
        Self::start_with_backoff(vars, allow_private, FIRST_BACKOFF)
    }

    /// [`LangfuseUploader::start`] with another first retry delay (tests).
    pub fn start_with_backoff(
        vars: &[(String, String)],
        allow_private: bool,
        backoff: Duration,
    ) -> Result<Self, CliError> {
        let usage = |e: String| CliError::usage(format!("--langfuse-scores: {e}"));
        let preset = graphrt_otel::preset::find("langfuse").map_err(|e| usage(e.to_string()))?;
        let resolved = preset.resolve(vars).map_err(|e| usage(e.to_string()))?;
        let authorization = resolved
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.clone())
            .ok_or_else(|| usage("no Langfuse credentials".to_owned()))?;
        let url = reqwest::Url::parse(&format!("{}/api/public/scores", resolved.base_url))
            .map_err(|e| usage(format!("invalid Langfuse URL: {e}")))?;
        let shown = redact_userinfo(url.as_str()).into_owned();
        if graphrt_core::urls::is_cleartext_remote(url.as_str()) {
            return Err(usage(format!(
                "{shown} is plain http:// to another machine; the Langfuse keys need https:// (or a loopback address)"
            )));
        }
        let policy = EgressPolicy { allow_private, max_redirects: 0, ..EgressPolicy::default() };
        let client = HttpClient::new(policy).map_err(|e| usage(e.to_string()))?;
        client.check_url(&url).map_err(|e| usage(format!("{shown}: {e} (see --allow-private-network)")))?;
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel(QUEUE);
        let target = Target { url: url.clone(), authorization, client, backoff };
        tokio::spawn(deliver_all(rx, target, Arc::clone(&counters)));
        Ok(Self { tx, counters, url: shown })
    }

    /// The scores endpoint, without userinfo (for logs).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Queue `score` of `record`'s run for upload. Never waits.
    pub fn submit(&self, record: &RunRecord, score: &Score) {
        let Some(body) = score_body(record, score) else {
            self.counters.untraced.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if self.tx.try_send(body).is_err() {
            let dropped = self.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                eprintln!("warning: Langfuse score upload queue full; {dropped} score(s) not uploaded so far");
            }
        }
    }

    /// Scores Langfuse accepted.
    pub fn sent(&self) -> u64 {
        self.counters.sent.load(Ordering::Relaxed)
    }

    /// Scores given up on after retries or a non-retryable answer.
    pub fn failed(&self) -> u64 {
        self.counters.failed.load(Ordering::Relaxed)
    }

    /// Scores not queued because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    /// Scores of runs without a trace id (not traced), never sent.
    pub fn untraced(&self) -> u64 {
        self.counters.untraced.load(Ordering::Relaxed)
    }
}

/// The `POST /api/public/scores` body, or `None` when the run has no trace id.
pub fn score_body(record: &RunRecord, score: &Score) -> Option<Json> {
    let trace_id = record.trace_id.as_deref()?;
    let (value, data_type) = match &score.value {
        ScoreValue::Number(n) => (json!(n), "NUMERIC"),
        ScoreValue::Bool(flag) => (json!(if *flag { 1 } else { 0 }), "BOOLEAN"),
        ScoreValue::Text(text) => (json!(text), "CATEGORICAL"),
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert("source".into(), score.source.as_str().into());
    metadata.insert("graphrt_run_id".into(), record.run_id.as_str().into());
    metadata.insert("graphrt_workflow".into(), record.workflow.as_str().into());
    for (key, field) in [("label", &score.label), ("type", &score.kind), ("user", &score.user)] {
        if let Some(text) = field {
            metadata.insert(key.into(), text.as_str().into());
        }
    }
    let mut body = json!({
        "id": score.id,
        "traceId": trace_id,
        "name": score.name,
        "value": value,
        "dataType": data_type,
        "metadata": metadata,
    });
    if let Some(object) = body.as_object_mut() {
        if let Some(span_id) = &record.span_id {
            object.insert("observationId".into(), span_id.as_str().into());
        }
        if let Some(comment) = &score.comment {
            object.insert("comment".into(), comment.chars().take(MAX_COMMENT_CHARS).collect::<String>().into());
        }
    }
    Some(body)
}

async fn deliver_all(mut rx: mpsc::Receiver<Json>, target: Target, counters: Arc<Counters>) {
    while let Some(body) = rx.recv().await {
        match deliver(&target, &body).await {
            Ok(()) => {
                counters.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                counters.failed.fetch_add(1, Ordering::Relaxed);
                let id = body.get("id").and_then(Json::as_str).unwrap_or_default();
                eprintln!("warning: Langfuse score {id} not uploaded: {error}");
            }
        }
    }
}

/// POST one score, retrying transient failures. Returns the last error.
async fn deliver(target: &Target, body: &Json) -> Result<(), String> {
    let client = target.client.manual_redirect_client(true).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let mut last_error = String::new();
    let mut delay = target.backoff;
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
        }
        let request = client
            .post(target.url.clone())
            .timeout(TIMEOUT)
            .header("content-type", "application/json")
            .header("authorization", &target.authorization)
            .header("user-agent", concat!("graphrt/", env!("CARGO_PKG_VERSION")))
            .body(bytes.clone());
        match request.send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let code = response.status().as_u16();
                last_error = format!("HTTP {code} (attempt {attempt})");
                if !(code == 408 || code == 429 || response.status().is_server_error()) {
                    return Err(last_error);
                }
            }
            Err(error) => last_error = format!("{} (attempt {attempt})", error.without_url()),
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scores::ScoreSource;

    #[test]
    fn bodies_map_value_types_and_need_a_trace() {
        let mut record = RunRecord::running("run-1", "chat", "fp", 1);
        let score = Score::new("run-1", "thumbs", ScoreValue::Bool(true), ScoreSource::Human, Some("u1".into()));
        assert!(score_body(&record, &score).is_none(), "untraced runs are not uploaded");
        record.trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".into());
        record.span_id = Some("00f067aa0ba902b7".into());
        let body = score_body(&record, &score).unwrap();
        assert_eq!(body["id"], score.id.as_str());
        assert_eq!((body["value"].clone(), body["dataType"].clone()), (json!(1), json!("BOOLEAN")));
        assert_eq!(body["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(body["observationId"], "00f067aa0ba902b7");
        assert_eq!(body["metadata"]["source"], "human");
        assert_eq!(body["metadata"]["user"], "u1");
        let text = Score::new("run-1", "topic", ScoreValue::Text("billing".into()), ScoreSource::Human, None);
        assert_eq!(score_body(&record, &text).unwrap()["dataType"], "CATEGORICAL");
        let mut online = Score::new("run-1", "judge", ScoreValue::Number(0.25), ScoreSource::Online, None);
        online.comment = Some("x".repeat(5000));
        let body = score_body(&record, &online).unwrap();
        assert_eq!(body["dataType"], "NUMERIC");
        assert_eq!(body["comment"].as_str().map(|c| c.chars().count()), Some(MAX_COMMENT_CHARS));
    }

    #[test]
    fn needs_the_langfuse_variables_and_a_public_url() {
        let vars = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = runtime.enter();
        let err = LangfuseUploader::start(&vars(&[("LANGFUSE_PUBLIC_KEY", "pk")]), false).unwrap_err();
        assert!(err.message.contains("LANGFUSE_SECRET_KEY"), "{}", err.message);
        let local = vars(&[
            ("LANGFUSE_PUBLIC_KEY", "pk"),
            ("LANGFUSE_SECRET_KEY", "sk"),
            ("LANGFUSE_BASE_URL", "http://127.0.0.1:3000"),
        ]);
        let err = LangfuseUploader::start(&local, false).unwrap_err();
        assert!(err.message.contains("--allow-private-network"), "{}", err.message);
        let uploader = LangfuseUploader::start(&local, true).unwrap();
        assert_eq!(uploader.url(), "http://127.0.0.1:3000/api/public/scores");
        let cloud = vars(&[("LANGFUSE_PUBLIC_KEY", "pk"), ("LANGFUSE_SECRET_KEY", "sk")]);
        assert_eq!(
            LangfuseUploader::start(&cloud, false).unwrap().url(),
            "https://cloud.langfuse.com/api/public/scores"
        );
        // A base URL naming the OTLP traces endpoint resolves to the same scores API.
        let full = vars(&[
            ("LANGFUSE_PUBLIC_KEY", "pk"),
            ("LANGFUSE_SECRET_KEY", "sk"),
            ("LANGFUSE_BASE_URL", "https://cloud.langfuse.com/api/public/otel/v1/traces"),
        ]);
        assert_eq!(
            LangfuseUploader::start(&full, false).unwrap().url(),
            "https://cloud.langfuse.com/api/public/scores"
        );
        // Keys never go over plain http to another machine, and URL credentials are not shown.
        let cleartext = vars(&[
            ("LANGFUSE_PUBLIC_KEY", "pk"),
            ("LANGFUSE_SECRET_KEY", "sk"),
            ("LANGFUSE_BASE_URL", "http://u:pw@lf.example"),
        ]);
        let err = LangfuseUploader::start(&cleartext, true).unwrap_err();
        assert!(err.message.contains("plain http://") && !err.message.contains("pw@"), "{}", err.message);
    }
}
