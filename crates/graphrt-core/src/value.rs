//! Runtime values.
//!
//! [`Value`] is the single dynamic value type flowing through a workflow. Large
//! payloads (strings, arrays, objects, files) are reference-counted, so cloning a
//! value — and therefore cloning a [`crate::pool::VariablePool`] — is O(1) per
//! entry instead of a deep copy.
//!
//! Secrets are a distinct variant. They render in plaintext only where a node
//! explicitly asks for the raw value ([`Value::expose_secret`]); every
//! serialization path (events, logs, snapshots) masks them.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

/// Ordered string-keyed map used for object values.
pub type Map = IndexMap<String, Value>;

/// Marker key used to serialize [`File`] values as plain JSON objects.
pub const FILE_MARKER_KEY: &str = "$file";

/// Text shown instead of a secret's contents.
pub const SECRET_MASK: &str = "******";

/// Marker key used for secrets inside snapshots: `{"$secret": "redacted"}`.
pub const SECRET_MARKER_KEY: &str = "$secret";

/// Internal contents of a redacted secret (a secret whose plaintext was not
/// persisted). Contains NUL bytes so it can never collide with a real value.
const REDACTED_SECRET: &str = "\u{0}graphrt:redacted-secret\u{0}";

/// Error returned when a secret's plaintext is required but unavailable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The secret was restored from a snapshot without its plaintext.
    #[error("secret is not available after resume; re-supply it in the resume input")]
    Redacted,
    /// The value is not a string or secret.
    #[error("expected a secret or string, got {0}")]
    NotText(&'static str),
}

/// A dynamic runtime value.
#[derive(Clone, Default, PartialEq)]
pub enum Value {
    /// Absent / null.
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Arc<str>),
    /// A sensitive string. Never serialized in plaintext.
    Secret(Arc<str>),
    Array(Arc<Vec<Value>>),
    Object(Arc<Map>),
    File(Arc<File>),
}

/// Declared type of a variable. Wire names are stable and used in workflow documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ValueType {
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "none")]
    Null,
    #[serde(rename = "string", alias = "text-input", alias = "paragraph", alias = "select")]
    String,
    #[serde(rename = "secret")]
    Secret,
    #[serde(rename = "number")]
    Number,
    #[serde(rename = "integer")]
    Integer,
    #[serde(rename = "float")]
    Float,
    #[serde(rename = "boolean", alias = "checkbox")]
    Boolean,
    #[serde(rename = "object", alias = "json")]
    Object,
    #[serde(rename = "file")]
    File,
    #[serde(rename = "array[any]", alias = "array")]
    ArrayAny,
    #[serde(rename = "array[string]")]
    ArrayString,
    #[serde(rename = "array[number]")]
    ArrayNumber,
    #[serde(rename = "array[boolean]")]
    ArrayBoolean,
    #[serde(rename = "array[object]")]
    ArrayObject,
    #[serde(rename = "array[file]", alias = "file-list")]
    ArrayFile,
}

/// Error returned when a value does not match a declared [`ValueType`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("expected {expected:?}, got {actual}")]
pub struct TypeMismatch {
    pub expected: ValueType,
    pub actual: &'static str,
}

impl ValueType {
    /// Element type for array types, `None` for scalars.
    pub fn element(self) -> Option<ValueType> {
        match self {
            ValueType::ArrayAny => Some(ValueType::Any),
            ValueType::ArrayString => Some(ValueType::String),
            ValueType::ArrayNumber => Some(ValueType::Number),
            ValueType::ArrayBoolean => Some(ValueType::Boolean),
            ValueType::ArrayObject => Some(ValueType::Object),
            ValueType::ArrayFile => Some(ValueType::File),
            _ => None,
        }
    }

    /// Whether this is one of the `array[...]` types.
    pub fn is_array(self) -> bool {
        self.element().is_some()
    }

    /// Zero value used when a typed variable has no explicit value.
    pub fn zero(self) -> Value {
        match self {
            ValueType::String => Value::from(""),
            ValueType::Secret => Value::Secret(Arc::from("")),
            ValueType::Number | ValueType::Integer => Value::Int(0),
            ValueType::Float => Value::Float(0.0),
            ValueType::Boolean => Value::Bool(false),
            ValueType::Object => Value::Object(Arc::default()),
            t if t.is_array() => Value::Array(Arc::default()),
            _ => Value::Null,
        }
    }

    /// Check `value` against this type and apply the only lossless coercions
    /// allowed: integer -> float for `float`, string -> secret for `secret`.
    /// Integers never silently absorb fractional floats.
    pub fn coerce(self, value: Value) -> Result<Value, TypeMismatch> {
        let mismatch = |v: &Value| TypeMismatch { expected: self, actual: v.type_name() };
        match (self, value) {
            (ValueType::Any, v) => Ok(v),
            (_, Value::Null) => Ok(Value::Null),
            (ValueType::String, v @ Value::String(_)) => Ok(v),
            (ValueType::Secret, Value::String(s) | Value::Secret(s)) => Ok(Value::Secret(s)),
            (ValueType::Number, v @ (Value::Int(_) | Value::Float(_))) => Ok(v),
            (ValueType::Integer, v @ Value::Int(_)) => Ok(v),
            (ValueType::Integer, Value::Float(f)) if f.fract() == 0.0 && f.is_finite() => {
                // Exact whole floats (e.g. `3.0` from JSON) are accepted; fractional ones are not.
                #[allow(clippy::cast_possible_truncation)]
                Ok(Value::Int(f as i64))
            }
            #[allow(clippy::cast_precision_loss)]
            (ValueType::Float, Value::Int(i)) => Ok(Value::Float(i as f64)),
            (ValueType::Float, v @ Value::Float(_)) => Ok(v),
            (ValueType::Boolean, v @ Value::Bool(_)) => Ok(v),
            (ValueType::Object, v @ Value::Object(_)) => Ok(v),
            (ValueType::File, v @ Value::File(_)) => Ok(v),
            (t, Value::Array(items)) if t.is_array() => {
                let elem = t.element().unwrap_or(ValueType::Any);
                if elem == ValueType::Any {
                    return Ok(Value::Array(items));
                }
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    match elem.coerce(item.clone()) {
                        Ok(v) => out.push(v),
                        Err(_) => return Err(TypeMismatch { expected: self, actual: "array with mismatched element" }),
                    }
                }
                Ok(Value::Array(Arc::new(out)))
            }
            (_, v) => Err(mismatch(&v)),
        }
    }
}

impl Value {
    /// Short type name used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Secret(_) => "secret",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::File(_) => "file",
        }
    }

    /// Infer the most specific declared type for this value.
    pub fn infer_type(&self) -> ValueType {
        match self {
            Value::Null => ValueType::Null,
            Value::Bool(_) => ValueType::Boolean,
            Value::Int(_) => ValueType::Integer,
            Value::Float(_) => ValueType::Float,
            Value::String(_) => ValueType::String,
            Value::Secret(_) => ValueType::Secret,
            Value::Object(_) => ValueType::Object,
            Value::File(_) => ValueType::File,
            Value::Array(items) => {
                let mut kind: Option<ValueType> = None;
                for item in items.iter() {
                    let t = match item.infer_type() {
                        ValueType::Integer | ValueType::Float => ValueType::Number,
                        other => other,
                    };
                    match kind {
                        None => kind = Some(t),
                        Some(k) if k == t => {}
                        Some(_) => return ValueType::ArrayAny,
                    }
                }
                match kind {
                    Some(ValueType::String) => ValueType::ArrayString,
                    Some(ValueType::Number) => ValueType::ArrayNumber,
                    Some(ValueType::Boolean) => ValueType::ArrayBoolean,
                    Some(ValueType::Object) => ValueType::ArrayObject,
                    Some(ValueType::File) => ValueType::ArrayFile,
                    _ => ValueType::ArrayAny,
                }
            }
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Raw string contents, including secrets. Use only where the plaintext is required
    /// (e.g. an outgoing HTTP header), never for logging.
    ///
    /// Returns `None` for a redacted secret (see [`Value::redacted_secret`]).
    pub fn expose_secret(&self) -> Option<&str> {
        match self {
            Value::Secret(s) if s.as_ref() == REDACTED_SECRET => None,
            Value::String(s) | Value::Secret(s) => Some(s),
            _ => None,
        }
    }

    /// Like [`Value::expose_secret`], but explains why the plaintext is unavailable.
    /// Nodes that must transmit a secret should use this so a redacted secret fails
    /// loudly instead of sending an empty or masked value.
    pub fn require_secret(&self) -> Result<&str, SecretError> {
        match self {
            Value::Secret(s) if s.as_ref() == REDACTED_SECRET => Err(SecretError::Redacted),
            Value::String(s) | Value::Secret(s) => Ok(s),
            other => Err(SecretError::NotText(other.type_name())),
        }
    }

    /// A secret whose plaintext is not available (restored from a snapshot).
    /// It renders masked, exposes nothing and fails [`Value::require_secret`].
    pub fn redacted_secret() -> Value {
        Value::Secret(Arc::from(REDACTED_SECRET))
    }

    /// Whether this is a [`Value::redacted_secret`].
    pub fn is_redacted_secret(&self) -> bool {
        matches!(self, Value::Secret(s) if s.as_ref() == REDACTED_SECRET)
    }

    /// Persistable form of this value: every secret (at any depth) becomes the
    /// marker object `{"$secret": "redacted"}`, so neither plaintext nor a mask
    /// string that could be mistaken for data is written to a snapshot.
    pub fn to_snapshot_value(&self) -> Value {
        match self {
            Value::Secret(_) => Value::object([(SECRET_MARKER_KEY, Value::from("redacted"))]),
            Value::Array(items) => Value::Array(Arc::new(items.iter().map(Value::to_snapshot_value).collect())),
            Value::Object(map) => {
                Value::Object(Arc::new(map.iter().map(|(k, v)| (k.clone(), v.to_snapshot_value())).collect()))
            }
            other => other.clone(),
        }
    }

    /// Inverse of [`Value::to_snapshot_value`]: secret markers become redacted secrets.
    pub fn from_snapshot_value(self) -> Value {
        match self {
            Value::Object(map) => {
                if map.len() == 1 && map.contains_key(SECRET_MARKER_KEY) {
                    return Value::redacted_secret();
                }
                let map = Arc::unwrap_or_clone(map);
                Value::Object(Arc::new(map.into_iter().map(|(k, v)| (k, v.from_snapshot_value())).collect()))
            }
            Value::Array(items) => {
                let items = Arc::unwrap_or_clone(items);
                Value::Array(Arc::new(items.into_iter().map(Value::from_snapshot_value).collect()))
            }
            other => other,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) if f.fract() == 0.0 && f.is_finite() =>
            {
                #[allow(clippy::cast_possible_truncation)]
                Some(*f as i64)
            }
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_file(&self) -> Option<&File> {
        match self {
            Value::File(f) => Some(f),
            _ => None,
        }
    }

    /// Whether the value is "empty" in the condition-operator sense.
    pub fn is_empty(&self) -> bool {
        match self {
            Value::Null => true,
            Value::String(s) | Value::Secret(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        }
    }

    /// Build an array value.
    pub fn array(items: impl IntoIterator<Item = Value>) -> Value {
        Value::Array(Arc::new(items.into_iter().collect()))
    }

    /// Build an object value.
    pub fn object<K: Into<String>>(entries: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Object(Arc::new(entries.into_iter().map(|(k, v)| (k.into(), v)).collect()))
    }

    /// Descend into the value along a path. Objects are indexed by key, arrays by
    /// decimal index (negative indexes count from the end), and files expose their
    /// attributes. Returns `None` when any hop is missing.
    pub fn get_path<S: AsRef<str>>(&self, path: &[S]) -> Option<Value> {
        let mut current = self.clone();
        for key in path {
            let key = key.as_ref();
            current = match &current {
                Value::Object(map) => map.get(key)?.clone(),
                Value::Array(items) => {
                    if key == "length" || key == "len" {
                        Value::Int(i64::try_from(items.len()).ok()?)
                    } else {
                        let idx: i64 = key.parse().ok()?;
                        let len = i64::try_from(items.len()).ok()?;
                        let idx = if idx < 0 { len + idx } else { idx };
                        let idx = usize::try_from(idx).ok()?;
                        items.get(idx)?.clone()
                    }
                }
                Value::File(file) => file.attribute(key)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Plain-text rendering used when a value is interpolated into a text template.
    ///
    /// Strings render raw, numbers and booleans in JSON form, objects/arrays as
    /// compact JSON, files as their URL or name, null as empty text, and secrets
    /// are masked.
    pub fn to_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::String(s) => s.to_string(),
            Value::Secret(_) => SECRET_MASK.to_owned(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::File(f) => f.url.clone().unwrap_or_else(|| f.name.clone()),
            Value::Array(_) | Value::Object(_) => serde_json::to_string(self).unwrap_or_default(),
        }
    }

    /// Like [`Value::to_text`], but reveals secrets. Only for nodes that must transmit
    /// the plaintext (HTTP auth headers, code sandbox inputs).
    /// A redacted secret renders as empty text.
    pub fn to_text_exposed(&self) -> String {
        match self {
            Value::Secret(s) if s.as_ref() == REDACTED_SECRET => String::new(),
            Value::Secret(s) => s.to_string(),
            other => other.to_text(),
        }
    }

    /// Convert from an **untrusted** JSON value (HTTP bodies, tool results, model
    /// output). Never creates [`Value::File`]: objects with a [`FILE_MARKER_KEY`]
    /// stay plain objects, so remote data cannot forge file references.
    pub fn from_json(json: serde_json::Value) -> Value {
        json_to_value(json, false)
    }

    /// Convert from **trusted** JSON (snapshots, host-supplied inputs). Objects
    /// carrying the [`FILE_MARKER_KEY`] that decode as a [`File`] become files.
    /// The [`Deserialize`] impl of [`Value`] follows the same trusted rule.
    pub fn from_json_trusted(json: serde_json::Value) -> Value {
        json_to_value(json, true)
    }
}

fn json_to_value(json: serde_json::Value, trusted: bool) -> Value {
    {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Value::String(Arc::from(s)),
            serde_json::Value::Array(items) => {
                Value::Array(Arc::new(items.into_iter().map(|v| json_to_value(v, trusted)).collect()))
            }
            serde_json::Value::Object(map) => {
                if trusted && map.contains_key(FILE_MARKER_KEY) {
                    if let Ok(file) = serde_json::from_value::<File>(serde_json::Value::Object(map.clone())) {
                        return Value::File(Arc::new(file));
                    }
                }
                Value::Object(Arc::new(map.into_iter().map(|(k, v)| (k, json_to_value(v, trusted))).collect()))
            }
        }
    }
}

impl Value {
    /// Convert to a JSON value. Secrets are masked.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Convert to JSON revealing secrets. Only for trusted sinks such as a code sandbox.
    /// A redacted secret becomes `null`.
    pub fn to_json_exposed(&self) -> serde_json::Value {
        match self {
            Value::Secret(s) if s.as_ref() == REDACTED_SECRET => serde_json::Value::Null,
            Value::Secret(s) => serde_json::Value::String(s.to_string()),
            Value::Array(items) => serde_json::Value::Array(items.iter().map(Value::to_json_exposed).collect()),
            Value::Object(map) => {
                serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_json_exposed())).collect())
            }
            other => other.to_json(),
        }
    }
}

fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { f.to_string() }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("Null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Secret(_) => write!(f, "Secret({SECRET_MASK})"),
            Value::Array(a) => f.debug_list().entries(a.iter()).finish(),
            Value::Object(o) => f.debug_map().entries(o.iter()).finish(),
            Value::File(file) => write!(f, "File({:?})", file.name),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(Arc::from(s))
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(Arc::from(s))
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i64::from(i))
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(Arc::new(v))
    }
}
impl From<Map> for Value {
    fn from(m: Map) -> Self {
        Value::Object(Arc::new(m))
    }
}
impl From<File> for Value {
    fn from(f: File) -> Self {
        Value::File(Arc::new(f))
    }
}
impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        Value::from_json(v)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::String(s) => serializer.serialize_str(s),
            Value::Secret(_) => serializer.serialize_str(SECRET_MASK),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items.iter() {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Object(map) => {
                let mut m = serializer.serialize_map(Some(map.len()))?;
                for (k, v) in map.iter() {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
            Value::File(file) => file.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON-compatible value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        Value::deserialize(d)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Int(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(match i64::try_from(v) {
            Ok(i) => Value::Int(i),
            #[allow(clippy::cast_precision_loss)]
            Err(_) => Value::Float(v as f64),
        })
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Float(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(v) = seq.next_element::<Value>()? {
            out.push(v);
        }
        Ok(Value::Array(Arc::new(out)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::with_capacity(access.size_hint().unwrap_or(0));
        while let Some((k, v)) = access.next_entry::<String, Value>()? {
            map.insert(k, v);
        }
        if map.contains_key(FILE_MARKER_KEY) {
            let json = Value::Object(Arc::new(map.clone())).to_json();
            if let Ok(file) = serde_json::from_value::<File>(json) {
                return Ok(Value::File(Arc::new(file)));
            }
        }
        Ok(Value::Object(Arc::new(map)))
    }
}

/// How a file's bytes are reachable. The runtime never reads file bytes itself;
/// nodes that need content ask a host-provided resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileTransfer {
    /// Publicly fetchable URL.
    #[default]
    RemoteUrl,
    /// Host-managed storage, addressed by `reference`.
    LocalFile,
    /// Produced by a tool; addressed by `reference`.
    ToolFile,
    /// Inline data URL / base64 payload in `url`.
    Inline,
}

/// Coarse file category used for routing (e.g. vision inputs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Image,
    Document,
    Audio,
    Video,
    #[default]
    Custom,
}

/// A reference to a file. Content is resolved by the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct File {
    /// Always `true`; marks this object as a file when serialized to JSON.
    #[serde(rename = "$file", default = "file_marker")]
    pub marker: bool,
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub kind: FileKind,
    #[serde(default)]
    pub transfer: FileTransfer,
    #[serde(default)]
    pub url: Option<String>,
    /// Opaque host storage key for `local_file` / `tool_file`.
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub extension: Option<String>,
}

fn file_marker() -> bool {
    true
}

impl File {
    /// Attribute lookup used by selectors and list/condition operators.
    pub fn attribute(&self, key: &str) -> Option<Value> {
        let opt = |s: &Option<String>| s.as_deref().map(Value::from).unwrap_or(Value::Null);
        Some(match key {
            "name" => Value::from(self.name.as_str()),
            "type" | "kind" => Value::from(enum_name(&self.kind)),
            "transfer_method" | "transfer" => Value::from(enum_name(&self.transfer)),
            "url" => opt(&self.url),
            "mime_type" => opt(&self.mime_type),
            "extension" => opt(&self.extension),
            "id" | "related_id" => opt(&self.id),
            "reference" => opt(&self.reference),
            "size" => self.size.and_then(|s| i64::try_from(s).ok()).map(Value::Int).unwrap_or(Value::Null),
            _ => return None,
        })
    }
}

fn enum_name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_rejects_fractional_float() {
        assert!(ValueType::Integer.coerce(Value::Float(1.5)).is_err());
        assert_eq!(ValueType::Integer.coerce(Value::Float(3.0)), Ok(Value::Int(3)));
    }

    #[test]
    fn number_rejects_bool() {
        assert!(ValueType::Number.coerce(Value::Bool(true)).is_err());
    }

    #[test]
    fn secret_is_masked_everywhere() {
        let v = Value::Secret(Arc::from("sk-123"));
        assert_eq!(v.to_text(), SECRET_MASK);
        assert_eq!(serde_json::to_string(&v).ok().as_deref(), Some("\"******\""));
        assert!(!format!("{v:?}").contains("sk-123"));
        assert_eq!(v.expose_secret(), Some("sk-123"));
    }

    #[test]
    fn path_supports_index_and_length() {
        let v = Value::from_json(serde_json::json!({"a": [1, {"b": "x"}]}));
        assert_eq!(v.get_path(&["a", "1", "b"]), Some(Value::from("x")));
        assert_eq!(v.get_path(&["a", "-1", "b"]), Some(Value::from("x")));
        assert_eq!(v.get_path(&["a", "length"]), Some(Value::Int(2)));
        assert_eq!(v.get_path(&["a", "9"]), None);
    }

    #[test]
    fn file_roundtrip_through_json() {
        let f = File { marker: true, name: "a.png".into(), kind: FileKind::Image, ..File::default() };
        let v = Value::from(f.clone());
        let json = serde_json::to_value(&v).ok();
        let back = json.clone().map(Value::from_json_trusted);
        assert_eq!(back, Some(Value::File(Arc::new(f))));
        // Untrusted JSON never forges files.
        let untrusted = json.map(Value::from_json);
        assert!(matches!(untrusted, Some(Value::Object(_))));
    }

    #[test]
    fn secrets_survive_snapshot_form_as_redacted() {
        let v = Value::object([("k", Value::Secret(Arc::from("sk-1"))), ("n", Value::Int(1))]);
        let snap = v.to_snapshot_value();
        let text = serde_json::to_string(&snap).unwrap();
        assert!(!text.contains("sk-1"));
        assert!(text.contains("\"$secret\""));
        let back: Value = serde_json::from_str::<Value>(&text).unwrap().from_snapshot_value();
        let k = back.get_path(&["k"]).unwrap();
        assert!(k.is_redacted_secret());
        assert_eq!(k.expose_secret(), None);
        assert_eq!(k.require_secret(), Err(SecretError::Redacted));
        assert_eq!(k.to_text(), SECRET_MASK);
        assert_eq!(k.to_text_exposed(), "");
        assert_eq!(back.get_path(&["n"]), Some(Value::Int(1)));
    }

    #[test]
    fn infer_array_types() {
        let v = Value::from_json(serde_json::json!([1, 2.5]));
        assert_eq!(v.infer_type(), ValueType::ArrayNumber);
        let v = Value::from_json(serde_json::json!([1, "a"]));
        assert_eq!(v.infer_type(), ValueType::ArrayAny);
    }

    #[test]
    fn text_rendering_is_json_like() {
        assert_eq!(Value::Bool(true).to_text(), "true");
        assert_eq!(Value::Float(1.0).to_text(), "1.0");
        assert_eq!(Value::from_json(serde_json::json!(["a", 1])).to_text(), "[\"a\",1]");
    }
}
