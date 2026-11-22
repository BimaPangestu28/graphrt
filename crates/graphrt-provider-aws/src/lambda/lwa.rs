//! Lambda Web Adapter framing.
//!
//! The gateway invokes the function with a Function URL (payload format 2.0)
//! event; Lambda Web Adapter turns it into a plain HTTP request to the agent.
//! With `AWS_LWA_INVOKE_MODE=response_stream` the reply is streamed as
//! `application/vnd.awslambda.http-integration-response`: a JSON prelude
//! (`statusCode`, `headers`, `cookies`), eight NUL bytes, then the raw body.
//! In the default buffered mode the reply is one JSON envelope
//! (`statusCode`, `headers`, `body`, `isBase64Encoded`) that arrives only when
//! the run is over, which [`HttpResponseDecoder`] detects (conformance C6).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

/// Separator between the streaming prelude and the body.
const PRELUDE_DELIMITER: [u8; 8] = [0; 8];
/// Lambda's buffered response limit is 6 MB; anything larger is malformed.
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;

/// One HTTP request for the agent behind Lambda Web Adapter.
#[derive(Clone, Debug, Default)]
pub struct AgentRequest {
    pub method: String,
    /// `/internal/v1/runs?x=1`.
    pub path_and_query: String,
    /// Header names are sent lower-cased; repeated names are joined with `,`.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Request id echoed in `requestContext.requestId` (the run id is a good choice).
    pub request_id: String,
}

/// The invoke payload: a Function URL (API Gateway HTTP API 2.0) event.
pub fn function_url_event(request: &AgentRequest, time_epoch_ms: u64) -> Value {
    let (path, query) = request.path_and_query.split_once('?').unwrap_or((&request.path_and_query, ""));
    let mut headers = serde_json::Map::new();
    for (name, value) in &request.headers {
        let name = name.to_ascii_lowercase();
        let joined = match headers.get(&name).and_then(Value::as_str) {
            Some(existing) => format!("{existing},{value}"),
            None => value.clone(),
        };
        headers.insert(name, Value::String(joined));
    }
    json!({
        "version": "2.0",
        "routeKey": "$default",
        "rawPath": path,
        "rawQueryString": query,
        "headers": headers,
        "requestContext": {
            "accountId": "anonymous",
            "apiId": "graphrt-gateway",
            "domainName": "graphrt-gateway.lambda",
            "domainPrefix": "graphrt-gateway",
            "http": {
                "method": request.method,
                "path": path,
                "protocol": "HTTP/1.1",
                "sourceIp": "127.0.0.1",
                "userAgent": "graphrt-gateway",
            },
            "requestId": request.request_id,
            "routeKey": "$default",
            "stage": "$default",
            "timeEpoch": time_epoch_ms,
        },
        "body": STANDARD.encode(&request.body),
        "isBase64Encoded": true,
    })
}

/// Status line and headers of the agent's reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// How the reply arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    /// Not decided yet (no body chunk seen).
    Unknown,
    /// Prelude + body chunks arrived incrementally.
    Streaming,
    /// Lambda Web Adapter in buffered mode (or a non-streaming handler): the whole
    /// reply arrived as one envelope at the end. SSE is useless in this mode.
    Buffered,
}

/// What one push into the decoder produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
}

/// Incremental decoder for the reply (prelude or envelope).
#[derive(Debug)]
pub struct HttpResponseDecoder {
    buffer: Vec<u8>,
    head_seen: bool,
    mode: StreamMode,
}

impl Default for HttpResponseDecoder {
    fn default() -> Self {
        Self { buffer: Vec::new(), head_seen: false, mode: StreamMode::Unknown }
    }
}

fn find_delimiter(bytes: &[u8]) -> Option<usize> {
    bytes.windows(PRELUDE_DELIMITER.len()).position(|w| w == PRELUDE_DELIMITER)
}

fn parse_headers(value: Option<&Value>, cookies: Option<&Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = value
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|v| (k.to_ascii_lowercase(), v.to_owned())))
        .collect();
    for cookie in cookies.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        out.push(("set-cookie".into(), cookie.to_owned()));
    }
    out
}

fn status_of(value: &Value) -> Result<u16, String> {
    value
        .get("statusCode")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| "Lambda Web Adapter reply has no valid statusCode".to_owned())
}

impl HttpResponseDecoder {
    pub fn mode(&self) -> StreamMode {
        self.mode
    }

    /// Feed payload bytes.
    ///
    /// @throws `String` when the prelude is not valid JSON or the reply is too large.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Decoded, String> {
        if self.head_seen {
            return Ok(Decoded { head: None, body: bytes.to_vec() });
        }
        self.buffer.extend_from_slice(bytes);
        if let Some(at) = find_delimiter(&self.buffer) {
            let prelude: Value = serde_json::from_slice(self.buffer.get(..at).unwrap_or_default())
                .map_err(|e| format!("Lambda Web Adapter prelude is not JSON: {e}"))?;
            let head = ResponseHead {
                status: status_of(&prelude)?,
                headers: parse_headers(prelude.get("headers"), prelude.get("cookies")),
            };
            let body = self.buffer.split_off(at + PRELUDE_DELIMITER.len());
            self.buffer.clear();
            self.head_seen = true;
            self.mode = StreamMode::Streaming;
            return Ok(Decoded { head: Some(head), body });
        }
        if self.buffer.len() > MAX_BUFFERED_BYTES {
            return Err(format!("Lambda reply exceeds {MAX_BUFFERED_BYTES} bytes without a streaming prelude"));
        }
        Ok(Decoded::default())
    }

    /// The invocation completed: decode a buffered envelope if no prelude was seen.
    ///
    /// @throws `String` when the bytes are neither a prelude nor an envelope.
    pub fn finish(&mut self) -> Result<Decoded, String> {
        if self.head_seen {
            return Ok(Decoded::default());
        }
        let envelope: Value = serde_json::from_slice(&self.buffer)
            .map_err(|_| "Lambda reply has neither a streaming prelude nor a JSON envelope".to_owned())?;
        let head = ResponseHead {
            status: status_of(&envelope)?,
            headers: parse_headers(envelope.get("headers"), envelope.get("cookies")),
        };
        let text = envelope.get("body").and_then(Value::as_str).unwrap_or_default();
        let body = if envelope.get("isBase64Encoded").and_then(Value::as_bool).unwrap_or(false) {
            STANDARD.decode(text).map_err(|e| format!("buffered body is not base64: {e}"))?
        } else {
            text.as_bytes().to_vec()
        };
        self.head_seen = true;
        self.mode = StreamMode::Buffered;
        self.buffer.clear();
        Ok(Decoded { head: Some(head), body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_shape() {
        let request = AgentRequest {
            method: "POST".into(),
            path_and_query: "/internal/v1/runs?from=3".into(),
            headers: vec![
                ("Graphrt-Sig".into(), "v1;kid=a".into()),
                ("X-A".into(), "1".into()),
                ("x-a".into(), "2".into()),
            ],
            body: b"{\"x\":1}".to_vec(),
            request_id: "run-1".into(),
        };
        let event = function_url_event(&request, 42);
        assert_eq!(event["rawPath"], "/internal/v1/runs");
        assert_eq!(event["rawQueryString"], "from=3");
        assert_eq!(event["headers"]["graphrt-sig"], "v1;kid=a");
        assert_eq!(event["headers"]["x-a"], "1,2");
        assert_eq!(event["requestContext"]["http"]["method"], "POST");
        assert_eq!(STANDARD.decode(event["body"].as_str().unwrap()).unwrap(), b"{\"x\":1}");
    }

    #[test]
    fn streaming_prelude_split_across_chunks() {
        let mut wire =
            br#"{"statusCode":200,"headers":{"Content-Type":"text/event-stream"},"cookies":["a=b"]}"#.to_vec();
        wire.extend_from_slice(&PRELUDE_DELIMITER);
        wire.extend_from_slice(b"event: hello\n\n");
        let mut d = HttpResponseDecoder::default();
        let mut head = None;
        let mut body = Vec::new();
        for chunk in wire.chunks(3) {
            let out = d.push(chunk).unwrap();
            head = head.or(out.head);
            body.extend(out.body);
        }
        let head = head.unwrap();
        assert_eq!(head.status, 200);
        assert!(head.headers.contains(&("content-type".into(), "text/event-stream".into())));
        assert!(head.headers.contains(&("set-cookie".into(), "a=b".into())));
        assert_eq!(body, b"event: hello\n\n");
        assert_eq!(d.mode(), StreamMode::Streaming);
        assert_eq!(d.push(b"more").unwrap().body, b"more");
    }

    #[test]
    fn buffered_envelope_is_detected() {
        let mut d = HttpResponseDecoder::default();
        let envelope = json!({"statusCode":200,"headers":{"content-type":"text/event-stream"},
            "body": STANDARD.encode("event: a\n\nevent: b\n\n"), "isBase64Encoded": true});
        assert_eq!(d.push(envelope.to_string().as_bytes()).unwrap(), Decoded::default());
        let out = d.finish().unwrap();
        assert_eq!(out.head.unwrap().status, 200);
        assert_eq!(out.body, b"event: a\n\nevent: b\n\n");
        assert_eq!(d.mode(), StreamMode::Buffered);
    }

    #[test]
    fn garbage_is_rejected() {
        let mut d = HttpResponseDecoder::default();
        d.push(b"not json").unwrap();
        assert!(d.finish().is_err());
        let mut d = HttpResponseDecoder::default();
        let mut wire = b"{oops".to_vec();
        wire.extend_from_slice(&PRELUDE_DELIMITER);
        assert!(d.push(&wire).is_err());
    }
}
