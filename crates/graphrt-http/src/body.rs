//! Request body construction.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use graphrt_core::node::NodeError;
use graphrt_core::template::{Part, Template};
use graphrt_core::value::File;
use graphrt_core::{Value, VariablePool};

/// Render a JSON body template safely.
///
/// Literal text is kept as written. Each placeholder is substituted according to
/// where it appears:
/// - inside a JSON string literal, the value's text is inserted JSON-escaped, so
///   quotes or braces inside a value cannot break out of the string;
/// - outside a string, the value's JSON encoding is inserted (`null` when missing).
///
/// The result must be valid JSON; otherwise a clear error is returned (no repair).
pub fn render_json(template: &Template, pool: &VariablePool) -> Result<serde_json::Value, NodeError> {
    let mut out = String::with_capacity(template.source().len());
    let mut in_string = false;
    let mut escaped = false;
    for part in template.parts() {
        match part {
            Part::Text(text) => {
                for ch in text.chars() {
                    if in_string {
                        if escaped {
                            escaped = false;
                        } else if ch == '\\' {
                            escaped = true;
                        } else if ch == '"' {
                            in_string = false;
                        }
                    } else if ch == '"' {
                        in_string = true;
                    }
                    out.push(ch);
                }
            }
            Part::Var(selector) => {
                let value = pool.get(selector).unwrap_or(Value::Null);
                if in_string {
                    let encoded = serde_json::to_string(&value.to_text_exposed())
                        .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode `{selector}`: {e}")))?;
                    // Strip the surrounding quotes of the encoded string literal.
                    let inner = encoded.get(1..encoded.len().saturating_sub(1)).unwrap_or_default();
                    out.push_str(inner);
                } else {
                    let encoded = serde_json::to_string(&value.to_json_exposed())
                        .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode `{selector}`: {e}")))?;
                    out.push_str(&encoded);
                }
            }
        }
    }
    if out.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&out)
        .map_err(|e| NodeError::fatal("invalid_input", format!("JSON body is not valid JSON after substitution: {e}")))
}

/// Raw bytes of an inline file (`data:` URL). Other transfer methods need host storage,
/// which the HTTP node does not access.
pub fn inline_file_bytes(file: &File) -> Result<Vec<u8>, NodeError> {
    let url = file.url.as_deref().unwrap_or_default();
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(NodeError::fatal(
            "unsupported_file",
            format!("file `{}` is not inline; only data: URL files can be sent by the HTTP node", file.name),
        ));
    };
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| NodeError::fatal("invalid_input", format!("file `{}` has a malformed data URL", file.name)))?;
    if meta.ends_with(";base64") {
        BASE64
            .decode(payload.trim())
            .map_err(|e| NodeError::fatal("invalid_input", format!("file `{}`: invalid base64: {e}", file.name)))
    } else {
        percent_decode(payload.as_bytes())
            .ok_or_else(|| NodeError::fatal("invalid_input", format!("file `{}`: invalid percent-encoding", file.name)))
    }
}

/// Decode `%XX` escapes (RFC 3986). Returns `None` on a malformed escape.
fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut iter = input.iter().copied();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = hex_value(iter.next()?)?;
            let lo = hex_value(iter.next()?)?;
            out.push(hi * 16 + lo);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode bytes as a `data:` URL.
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", BASE64.encode(bytes))
}

/// A part of a `multipart/form-data` body.
pub enum MultipartField {
    Text { name: String, value: String },
    File { name: String, filename: String, mime: String, bytes: Vec<u8> },
}

/// Encode a multipart body. Returns `(content_type, body)`.
pub fn encode_multipart(fields: &[MultipartField], boundary: &str) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\r', '\n'], " ");
    for field in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match field {
            MultipartField::Text { name, value } => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", quote(name)).as_bytes(),
                );
                body.extend_from_slice(value.as_bytes());
            }
            MultipartField::File { name, filename, mime, bytes } => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                        quote(name),
                        quote(filename),
                        mime.replace(['\r', '\n'], "")
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(bytes);
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("n", "evil", Value::from("x\", \"admin\": true, \"y\": \"{"));
        p.insert("n", "num", Value::Int(42));
        p.insert("n", "obj", Value::from_json(serde_json::json!({"a": [1, "b"]})));
        p.insert("n", "quote", Value::from("he said \"hi\"\n"));
        p.insert("env", "KEY", Value::Secret(std::sync::Arc::from("sk-1")));
        p
    }

    #[test]
    fn injection_inside_string_is_escaped() {
        let t = Template::parse(r#"{"name": "{{#n.evil#}}", "admin": false}"#);
        let v = render_json(&t, &pool()).unwrap();
        assert_eq!(v["admin"], serde_json::json!(false));
        assert_eq!(v["name"], serde_json::json!("x\", \"admin\": true, \"y\": \"{"));
        assert_eq!(v.as_object().map(|o| o.len()), Some(2));
    }

    #[test]
    fn bare_placeholders_insert_typed_json() {
        let t = Template::parse(r#"{"n": {{#n.num#}}, "o": {{#n.obj#}}, "missing": {{#n.nope#}}}"#);
        let v = render_json(&t, &pool()).unwrap();
        assert_eq!(v, serde_json::json!({"n": 42, "o": {"a": [1, "b"]}, "missing": null}));
    }

    #[test]
    fn bare_string_placeholder_is_quoted() {
        let t = Template::parse(r#"{"s": {{#n.evil#}}}"#);
        let v = render_json(&t, &pool()).unwrap();
        assert_eq!(v["s"], serde_json::json!("x\", \"admin\": true, \"y\": \"{"));
    }

    #[test]
    fn escaped_quotes_in_literal_text_keep_string_state() {
        let t = Template::parse(r#"{"s": "a\"b {{#n.quote#}}"}"#);
        let v = render_json(&t, &pool()).unwrap();
        assert_eq!(v["s"], serde_json::json!("a\"b he said \"hi\"\n"));
    }

    #[test]
    fn secrets_are_revealed_in_body() {
        let t = Template::parse(r#"{"key": "{{#env.KEY#}}"}"#);
        assert_eq!(render_json(&t, &pool()).unwrap()["key"], serde_json::json!("sk-1"));
    }

    #[test]
    fn invalid_json_fails_without_repair() {
        let t = Template::parse(r#"{"a": 1,}"#);
        let err = render_json(&t, &pool()).unwrap_err();
        assert_eq!(err.kind, "invalid_input");
        assert!(!err.retryable);
    }

    #[test]
    fn data_url_roundtrip() {
        let url = data_url("text/plain", b"hello");
        let f = File { url: Some(url), name: "a.txt".into(), ..File::default() };
        assert_eq!(inline_file_bytes(&f).unwrap(), b"hello");
        let remote = File { url: Some("https://x/y".into()), name: "y".into(), ..File::default() };
        assert!(inline_file_bytes(&remote).is_err());
    }

    #[test]
    fn plain_data_url_is_percent_decoded() {
        let f = File { url: Some("data:text/plain,a%20b%2Cc".into()), name: "a.txt".into(), ..File::default() };
        assert_eq!(inline_file_bytes(&f).unwrap(), b"a b,c");
        let bad = File { url: Some("data:text/plain,%zz".into()), name: "b.txt".into(), ..File::default() };
        assert!(inline_file_bytes(&bad).is_err());
    }

    #[test]
    fn multipart_encoding() {
        let (ct, body) = encode_multipart(
            &[
                MultipartField::Text { name: "a\"b".into(), value: "v".into() },
                MultipartField::File {
                    name: "f".into(),
                    filename: "x.bin".into(),
                    mime: "application/octet-stream".into(),
                    bytes: vec![1, 2],
                },
            ],
            "BOUND",
        );
        assert_eq!(ct, "multipart/form-data; boundary=BOUND");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"a\\\"b\""));
        assert!(text.contains("filename=\"x.bin\""));
        assert!(text.ends_with("--BOUND--\r\n"));
    }
}
