//! Selectors and the variable pool.
//!
//! A [`Selector`] addresses a value as `namespace.name[.path...]`, where the
//! namespace is a node id or one of the reserved namespaces ([`SYS`], [`ENV`],
//! [`CONVERSATION`]). The [`VariablePool`] stores one map per namespace behind an
//! `Arc`, so cloning a pool (for an iteration item or loop round) is cheap and
//! writes are copy-on-write per namespace.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::value::{Value, ValueType};

/// System variables (`sys.query`, `sys.files`, `sys.run_id`, ...). Written by the host only.
pub const SYS: &str = "sys";
/// Environment variables / secrets. Supplied by the host, never persisted in snapshots.
pub const ENV: &str = "env";
/// Conversation-scoped variables. Persisted by the host between runs.
pub const CONVERSATION: &str = "conversation";

/// Address of a value: `[namespace, name, path...]`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Selector {
    parts: Arc<[Arc<str>]>,
}

/// Error for malformed selectors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid selector `{0}`: needs at least a namespace and a name")]
pub struct InvalidSelector(pub String);

impl Selector {
    /// Build from parts. Requires at least two non-empty parts.
    pub fn new<I, S>(parts: I) -> Result<Self, InvalidSelector>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parts: Vec<Arc<str>> = parts.into_iter().map(|p| Arc::from(p.as_ref())).collect();
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
            return Err(InvalidSelector(parts.join(".")));
        }
        Ok(Self { parts: parts.into() })
    }

    /// Parse a dotted selector such as `llm.text` or `start.user.name`.
    pub fn parse(dotted: &str) -> Result<Self, InvalidSelector> {
        Self::new(dotted.split('.'))
    }

    /// Two-part selector `namespace.name`.
    pub fn of(namespace: &str, name: &str) -> Self {
        Self { parts: vec![Arc::from(namespace), Arc::from(name)].into() }
    }

    pub fn namespace(&self) -> &str {
        self.parts.first().map(AsRef::as_ref).unwrap_or_default()
    }

    pub fn name(&self) -> &str {
        self.parts.get(1).map(AsRef::as_ref).unwrap_or_default()
    }

    /// Path below the variable (`a.b.c` -> `[c]`).
    pub fn path(&self) -> &[Arc<str>] {
        self.parts.get(2..).unwrap_or_default()
    }

    pub fn parts(&self) -> &[Arc<str>] {
        &self.parts
    }

    /// The `namespace.name` prefix of this selector.
    pub fn root(&self) -> Selector {
        Selector::of(self.namespace(), self.name())
    }

    /// Append path parts.
    pub fn child(&self, part: &str) -> Selector {
        let mut parts: Vec<Arc<str>> = self.parts.to_vec();
        parts.push(Arc::from(part));
        Selector { parts: parts.into() }
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.parts.join("."))
    }
}

impl fmt::Debug for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Selector({self})")
    }
}

impl Serialize for Selector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.parts.iter().map(AsRef::as_ref).collect::<Vec<&str>>().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Selector {
    /// Accepts `["node", "var"]` or `"node.var"`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            List(Vec<String>),
            Dotted(String),
        }
        let sel = match Repr::deserialize(deserializer)? {
            Repr::List(parts) => Selector::new(parts),
            Repr::Dotted(s) => Selector::parse(&s),
        };
        sel.map_err(serde::de::Error::custom)
    }
}

type Namespace = Arc<IndexMap<Arc<str>, Value>>;

/// Variable storage for one execution frame.
#[derive(Clone, Default)]
pub struct VariablePool {
    namespaces: HashMap<Arc<str>, Namespace>,
}

impl fmt::Debug for VariablePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (ns, vars) in &self.namespaces {
            map.entry(ns, vars);
        }
        map.finish()
    }
}

impl VariablePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a selector, descending into nested values. Missing values return `None`.
    pub fn get(&self, selector: &Selector) -> Option<Value> {
        let root = self.namespaces.get(selector.namespace())?.get(selector.name())?;
        if selector.path().is_empty() {
            Some(root.clone())
        } else {
            root.get_path(selector.path())
        }
    }

    /// Resolve a selector, returning [`Value::Null`] when absent.
    pub fn get_or_null(&self, selector: &Selector) -> Value {
        self.get(selector).unwrap_or(Value::Null)
    }

    /// Set `namespace.name`. Any path in the selector is ignored; use [`VariablePool::set_path`]
    /// for nested writes.
    pub fn set(&mut self, selector: &Selector, value: Value) {
        self.insert(selector.namespace(), selector.name(), value);
    }

    /// Set a variable by namespace and name.
    pub fn insert(&mut self, namespace: &str, name: &str, value: Value) {
        let ns = self.namespaces.entry(Arc::from(namespace)).or_default();
        Arc::make_mut(ns).insert(Arc::from(name), value);
    }

    /// Write into a nested object path, creating intermediate objects as needed.
    pub fn set_path(&mut self, selector: &Selector, value: Value) {
        if selector.path().is_empty() {
            self.set(selector, value);
            return;
        }
        let current = self.get(&selector.root()).unwrap_or(Value::Null);
        let updated = write_path(current, selector.path(), value);
        self.set(selector, updated);
    }

    /// Remove one variable.
    pub fn remove(&mut self, selector: &Selector) -> Option<Value> {
        let ns = self.namespaces.get_mut(selector.namespace())?;
        Arc::make_mut(ns).shift_remove(selector.name())
    }

    /// Drop a whole namespace (e.g. a node's outputs before a loop round re-runs it).
    pub fn remove_namespace(&mut self, namespace: &str) {
        self.namespaces.remove(namespace);
    }

    /// Bulk-insert a node's outputs.
    pub fn set_outputs(&mut self, namespace: &str, outputs: &IndexMap<String, Value>) {
        let ns = self.namespaces.entry(Arc::from(namespace)).or_default();
        let ns = Arc::make_mut(ns);
        for (k, v) in outputs {
            ns.insert(Arc::from(k.as_str()), v.clone());
        }
    }

    /// All variables in a namespace.
    pub fn namespace(&self, namespace: &str) -> Option<&IndexMap<Arc<str>, Value>> {
        self.namespaces.get(namespace).map(AsRef::as_ref)
    }

    pub fn contains(&self, selector: &Selector) -> bool {
        self.get(selector).is_some()
    }

    /// Iterate `(namespace, name, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.namespaces
            .iter()
            .flat_map(|(ns, vars)| vars.iter().map(move |(k, v)| (ns.as_ref(), k.as_ref(), v)))
    }

    /// Serializable form for snapshots. The [`ENV`] namespace is excluded: the host
    /// must supply environment variables and secrets again on resume.
    pub fn to_snapshot(&self) -> PoolSnapshot {
        let mut out: IndexMap<String, IndexMap<String, Value>> = IndexMap::new();
        let mut names: Vec<&Arc<str>> = self.namespaces.keys().collect();
        names.sort();
        for ns in names {
            if ns.as_ref() == ENV {
                continue;
            }
            if let Some(vars) = self.namespaces.get(ns) {
                out.insert(ns.to_string(), vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
            }
        }
        PoolSnapshot(out)
    }

    /// Restore from a snapshot (env variables must be re-applied by the caller).
    pub fn from_snapshot(snapshot: PoolSnapshot) -> Self {
        let mut pool = Self::new();
        for (ns, vars) in snapshot.0 {
            for (k, v) in vars {
                pool.insert(&ns, &k, v);
            }
        }
        pool
    }

    /// Insert a typed declared variable, validating it against its declared type.
    pub fn insert_typed(
        &mut self,
        namespace: &str,
        name: &str,
        ty: ValueType,
        value: Value,
    ) -> Result<(), crate::value::TypeMismatch> {
        let v = ty.coerce(value)?;
        self.insert(namespace, name, v);
        Ok(())
    }
}

/// Persistable variable pool contents (`namespace -> name -> value`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PoolSnapshot(pub IndexMap<String, IndexMap<String, Value>>);

fn write_path(current: Value, path: &[Arc<str>], value: Value) -> Value {
    let Some((head, rest)) = path.split_first() else {
        return value;
    };
    match current {
        Value::Array(items) if head.parse::<usize>().is_ok() => {
            let idx = head.parse::<usize>().unwrap_or(0);
            let mut items = Arc::unwrap_or_clone(items);
            if let Some(slot) = items.get_mut(idx) {
                let old = std::mem::take(slot);
                *slot = write_path(old, rest, value);
            }
            Value::Array(Arc::new(items))
        }
        other => {
            let mut map = match other {
                Value::Object(m) => Arc::unwrap_or_clone(m),
                _ => IndexMap::new(),
            };
            let old = map.shift_remove(head.as_ref()).unwrap_or(Value::Null);
            map.insert(head.to_string(), write_path(old, rest, value));
            Value::Object(Arc::new(map))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(s: &str) -> Selector {
        Selector::parse(s).unwrap_or_else(|_| Selector::of("bad", "bad"))
    }

    #[test]
    fn selector_parsing() {
        assert!(Selector::parse("a").is_err());
        assert!(Selector::parse("a..b").is_err());
        let s = sel("n.v.x.y");
        assert_eq!(s.namespace(), "n");
        assert_eq!(s.name(), "v");
        assert_eq!(s.path().len(), 2);
        let from_list: Result<Selector, _> = serde_json::from_str("[\"n\",\"v\"]");
        let from_str: Result<Selector, _> = serde_json::from_str("\"n.v\"");
        assert_eq!(from_list.ok(), from_str.ok());
    }

    #[test]
    fn clone_is_copy_on_write() {
        let mut a = VariablePool::new();
        a.insert("n", "x", Value::Int(1));
        let mut b = a.clone();
        b.insert("n", "x", Value::Int(2));
        assert_eq!(a.get(&sel("n.x")), Some(Value::Int(1)));
        assert_eq!(b.get(&sel("n.x")), Some(Value::Int(2)));
    }

    #[test]
    fn nested_get_and_set() {
        let mut p = VariablePool::new();
        p.set_path(&sel("n.obj.a.b"), Value::from("deep"));
        assert_eq!(p.get(&sel("n.obj.a.b")), Some(Value::from("deep")));
        assert_eq!(p.get(&sel("n.obj.missing")), None);
    }

    #[test]
    fn snapshot_excludes_env() {
        let mut p = VariablePool::new();
        p.insert(ENV, "KEY", Value::Secret(Arc::from("s3cret")));
        p.insert("n", "x", Value::Int(1));
        let snap = p.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap_or_default();
        assert!(!json.contains("s3cret"));
        assert!(!json.contains("env"));
        let restored = VariablePool::from_snapshot(snap);
        assert_eq!(restored.get(&sel("n.x")), Some(Value::Int(1)));
    }
}
