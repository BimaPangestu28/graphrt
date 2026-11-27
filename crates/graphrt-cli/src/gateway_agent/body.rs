//! Request bodies of `/internal/v1`, decoded by hand from a JSON tree.
//!
//! The tree comes from `serde_json::from_str::<Value>`, which the binary
//! already has, and the snapshot from [`Snapshot::from_json`] (the run store's
//! decoder). A `#[derive(Deserialize)]` struct per body would add its own copy
//! of every field decoder; for a feature that must stay small, picking fields
//! out of the tree is the cheaper way to read the same JSON.

use graphrt_core::Snapshot;
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Why a body was refused (a `400 bad_request` message).
pub type BodyError = &'static str;

/// Body of `POST /internal/v1/runs`: the fields of a public run request
/// (`inputs`, `sys`, `conversation`, `metadata`, `session_id`, `user_id`,
/// `tags`) plus the gateway's own.
#[derive(Debug, Default)]
pub struct RunBody {
    /// Served workflow name.
    pub workflow: String,
    /// Expected graph fingerprint (`409 fingerprint_mismatch` on another one).
    pub fingerprint: Option<String>,
    /// Deployment version, copied to the run record.
    pub deployment_version: Option<String>,
    /// Global index of this execution's first event (the gateway's event count).
    pub event_index: u64,
    /// Resume a paused run instead of starting it.
    pub resume: Option<ResumeBody>,
    /// Preempt the execution after this many milliseconds (the gateway sets it
    /// shortly before the platform's request cap, e.g. Cloud Run's 60 minutes):
    /// the run pauses with reason kind `external` / `request_cap` and the gateway
    /// re-dispatches it, instead of the platform killing the request.
    pub preempt_after_ms: Option<u64>,
    pub inputs: Map<String, Value>,
    pub sys: Map<String, Value>,
    pub conversation: Map<String, Value>,
    pub metadata: IndexMap<String, String>,
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    /// Who started the run, as the gateway says: `user` (an end user whose
    /// token it verified, named in `user_id`) or `key`. `None` from a gateway
    /// older than the field.
    pub caller: Option<String>,
}

/// `resume` of [`RunBody`].
#[derive(Debug)]
pub struct ResumeBody {
    pub snapshot: Snapshot,
    /// Input for paused nodes keyed by node id (or `<frame>/<node_id>`).
    pub inputs: Map<String, Value>,
    /// Secrets (other than `env.*`) to re-supply, keyed by `namespace.name`.
    pub secrets: IndexMap<String, String>,
}

/// The top-level object of a JSON body (an empty body is an empty object).
///
/// @throws BodyError - When the body is not UTF-8 JSON or not an object.
pub fn object(body: &[u8]) -> Result<Map<String, Value>, BodyError> {
    if body.is_empty() {
        return Ok(Map::new());
    }
    let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8")?;
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("body must be a JSON object"),
        Err(_) => Err("body is not valid JSON"),
    }
}

/// Decode `POST /internal/v1/runs`.
///
/// @throws BodyError - On a missing `workflow` or a field of the wrong type.
pub fn run(body: &[u8]) -> Result<RunBody, BodyError> {
    let mut map = object(body)?;
    let mut run = RunBody {
        workflow: string(&mut map, "workflow")?.ok_or("`workflow` is required")?,
        fingerprint: string(&mut map, "fingerprint")?,
        deployment_version: string(&mut map, "deployment_version")?,
        event_index: match map.remove("event_index") {
            None | Some(Value::Null) => 0,
            Some(value) => value.as_u64().ok_or("`event_index` must be a non-negative integer")?,
        },
        preempt_after_ms: match map.remove("preempt_after_ms") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or("`preempt_after_ms` must be a non-negative integer")?),
        },
        inputs: object_field(&mut map, "inputs")?,
        sys: object_field(&mut map, "sys")?,
        conversation: object_field(&mut map, "conversation")?,
        metadata: strings(object_field(&mut map, "metadata")?)?,
        session_id: string(&mut map, "session_id")?,
        user_id: string(&mut map, "user_id")?,
        caller: string(&mut map, "caller")?,
        ..RunBody::default()
    };
    run.tags = match map.remove("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.into_iter().map(into_string).collect::<Option<_>>().ok_or(TAGS)?,
        Some(_) => return Err(TAGS),
    };
    run.resume = match map.remove("resume") {
        None | Some(Value::Null) => None,
        Some(Value::Object(mut resume)) => {
            let snapshot = resume.remove("snapshot").ok_or("`resume.snapshot` is required")?;
            // Through the run store's decoder: one copy of the snapshot types' code.
            let snapshot = Snapshot::from_json(&snapshot.to_string()).map_err(|_| "invalid `resume.snapshot`")?;
            Some(ResumeBody {
                snapshot,
                inputs: object_field(&mut resume, "inputs")?,
                secrets: strings(object_field(&mut resume, "secrets")?)?,
            })
        }
        Some(_) => return Err("`resume` must be an object"),
    };
    Ok(run)
}

const TAGS: BodyError = "`tags` must be an array of strings";

/// `deadline_ms` of `POST /internal/v1/drain`, if given.
///
/// @throws BodyError - When the body is not an object or the field not an integer.
pub fn drain_deadline_ms(body: &[u8]) -> Result<Option<u64>, BodyError> {
    match object(body)?.remove("deadline_ms") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or("`deadline_ms` must be a non-negative integer"),
    }
}

fn into_string(value: Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text),
        _ => None,
    }
}

fn string(map: &mut Map<String, Value>, key: &'static str) -> Result<Option<String>, BodyError> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err("a string field has another type"),
    }
}

fn object_field(map: &mut Map<String, Value>, key: &'static str) -> Result<Map<String, Value>, BodyError> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(object)) => Ok(object),
        Some(_) => Err("an object field has another type"),
    }
}

fn strings(map: Map<String, Value>) -> Result<IndexMap<String, String>, BodyError> {
    map.into_iter().map(|(k, v)| into_string(v).map(|v| (k, v))).collect::<Option<_>>().ok_or("values must be strings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bodies_are_decoded() {
        let body = serde_json::json!({
            "workflow": "chat", "fingerprint": "fp", "event_index": 7, "deployment_version": "v2",
            "inputs": { "q": 1 }, "sys": { "s": true }, "metadata": { "tenant": "acme" },
            "session_id": "s1", "user_id": "u1", "tags": ["a", "b"], "stream": true, "caller": "user",
        });
        let run = run(body.to_string().as_bytes()).unwrap();
        assert_eq!((run.workflow.as_str(), run.fingerprint.as_deref(), run.event_index), ("chat", Some("fp"), 7));
        assert_eq!(run.deployment_version.as_deref(), Some("v2"));
        assert_eq!((run.inputs.len(), run.sys.len(), run.conversation.len()), (1, 1, 0));
        assert_eq!(run.metadata.get("tenant").map(String::as_str), Some("acme"));
        assert_eq!((run.session_id.as_deref(), run.user_id.as_deref()), (Some("s1"), Some("u1")));
        assert_eq!(run.tags, ["a", "b"]);
        assert_eq!(run.caller.as_deref(), Some("user"));
        assert!(run.resume.is_none());
    }

    #[test]
    fn bad_bodies_are_refused() {
        for bad in [
            "[]",
            "nope",
            r#"{}"#,
            r#"{"workflow": 1}"#,
            r#"{"workflow": "w", "event_index": -1}"#,
            r#"{"workflow": "w", "inputs": []}"#,
            r#"{"workflow": "w", "tags": [1]}"#,
            r#"{"workflow": "w", "metadata": {"k": 1}}"#,
            r#"{"workflow": "w", "resume": {}}"#,
            r#"{"workflow": "w", "resume": {"snapshot": 1}}"#,
            r#"{"workflow": "w", "resume": 1}"#,
            r#"{"workflow": "w", "preempt_after_ms": "soon"}"#,
            r#"{"workflow": "w", "caller": true}"#,
        ] {
            assert!(run(bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(run(&[0xff]).is_err());
    }

    #[test]
    fn drain_deadlines_are_optional() {
        assert_eq!(drain_deadline_ms(b""), Ok(None));
        assert_eq!(drain_deadline_ms(br#"{"deadline_ms": 500}"#), Ok(Some(500)));
        assert!(drain_deadline_ms(br#"{"deadline_ms": "soon"}"#).is_err());
    }
}
