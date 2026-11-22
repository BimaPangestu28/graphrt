//! Small JSON objects for gateway protocol responses, written as text.
//!
//! Building them with `serde_json::json!` instantiates a value conversion per
//! field type; writing the text directly needs only the string escaper the
//! binary already has (`serde_json::to_string::<str>`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// A JSON object written field by field.
#[derive(Debug)]
pub struct Object(String);

impl Object {
    pub fn new() -> Self {
        Self(String::from("{"))
    }

    fn key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        push_str(&mut self.0, key);
        self.0.push(':');
    }

    /// A string field.
    pub fn str(mut self, key: &str, value: &str) -> Self {
        self.key(key);
        push_str(&mut self.0, value);
        self
    }

    /// A number field.
    pub fn num(mut self, key: &str, value: u64) -> Self {
        self.key(key);
        self.0.push_str(&value.to_string());
        self
    }

    /// A boolean field.
    pub fn bool(mut self, key: &str, value: bool) -> Self {
        self.key(key);
        self.0.push_str(if value { "true" } else { "false" });
        self
    }

    /// A field holding already-encoded JSON.
    pub fn raw(mut self, key: &str, json: &str) -> Self {
        self.key(key);
        self.0.push_str(json);
        self
    }

    /// The object's text.
    pub fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

impl Default for Object {
    fn default() -> Self {
        Self::new()
    }
}

/// `text` as a JSON string literal.
pub fn push_str(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned()));
}

/// A JSON array of already-encoded JSON values.
pub fn array<I: IntoIterator<Item = String>>(items: I) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&item);
    }
    out.push(']');
    out
}

/// `body` (JSON text) with `status`.
pub fn response(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_are_valid_json() {
        let text = Object::new()
            .str("name", "a \"quoted\"\nline")
            .num("n", 7)
            .bool("ok", true)
            .raw("list", &array(["1".to_owned(), "{}".to_owned()]))
            .finish();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "name": "a \"quoted\"\nline", "n": 7, "ok": true, "list": [1, {}] }));
        assert_eq!(Object::new().finish(), "{}");
    }
}
