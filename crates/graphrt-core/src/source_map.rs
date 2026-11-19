//! Source positions of the values in a workflow document.
//!
//! A [`SourceMap`] maps document paths (`graph.nodes[1].template`, see
//! [`DocPath`]) to the line and column of the YAML or JSON node they came
//! from, so validation errors can point at the offending text. It is built by
//! a second, span-aware parse of the same text and is best effort: when the
//! text cannot be parsed, or the `yaml` feature (which provides the span-aware
//! parser, also used for JSON) is disabled, the map is empty and diagnostics
//! carry a path but no position.

use std::collections::HashMap;
use std::fmt;

use serde::Serialize;

/// A position in the source text. Lines and columns are 1-based and count
/// characters, not bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SourceSpan {
    pub line: u32,
    pub column: u32,
    /// Length of the node in characters (at least 1). For multi-line nodes
    /// this runs past the end of the first line.
    pub len: u32,
}

/// One segment of a [`DocPath`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// Path of a value inside a workflow document, e.g. `graph.nodes[1].template`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DocPath(Vec<PathSegment>);

impl DocPath {
    /// The document root.
    pub fn root() -> Self {
        Self::default()
    }

    /// This path extended by an object key.
    #[must_use]
    pub fn key(&self, key: &str) -> Self {
        let mut segments = self.0.clone();
        segments.push(PathSegment::Key(key.to_owned()));
        Self(segments)
    }

    /// This path extended by an array index.
    #[must_use]
    pub fn index(&self, index: usize) -> Self {
        let mut segments = self.0.clone();
        segments.push(PathSegment::Index(index));
        Self(segments)
    }

    /// The enclosing path, `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        let (_, rest) = self.0.split_last()?;
        Some(Self(rest.to_vec()))
    }

    /// The segments, outermost first.
    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }

    /// The last key, if the path ends in a key.
    pub fn last_key(&self) -> Option<&str> {
        match self.0.last() {
            Some(PathSegment::Key(key)) => Some(key),
            _ => None,
        }
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for DocPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("<document>");
        }
        for (i, segment) in self.0.iter().enumerate() {
            match segment {
                PathSegment::Index(ix) => write!(f, "[{ix}]")?,
                PathSegment::Key(key) if is_plain_key(key) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(key)?;
                }
                PathSegment::Key(key) => write!(f, "[{key:?}]")?,
            }
        }
        Ok(())
    }
}

impl Serialize for DocPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn is_plain_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Positions of keys and values, by document path.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    /// Position of the value at a path.
    values: HashMap<DocPath, SourceSpan>,
    /// Position of the mapping key introducing a path (object members only).
    keys: HashMap<DocPath, SourceSpan>,
}

impl SourceMap {
    /// An empty map (no positions known).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Positions for a JSON or YAML document. Empty when the text cannot be
    /// parsed or the `yaml` feature is disabled.
    pub fn from_text(text: &str) -> Self {
        #[cfg(feature = "yaml")]
        {
            spanned::parse(text).unwrap_or_default()
        }
        #[cfg(not(feature = "yaml"))]
        {
            let _ = text;
            Self::default()
        }
    }

    /// Position of the value at `path`.
    pub fn value(&self, path: &DocPath) -> Option<SourceSpan> {
        self.values.get(path).copied()
    }

    /// Position of the key that introduces `path` (for object members).
    pub fn key(&self, path: &DocPath) -> Option<SourceSpan> {
        self.keys.get(path).copied()
    }

    /// Best position for a diagnostic about `path`: the key (when `prefer_key`)
    /// or the value at the path, else the closest enclosing value that has one.
    pub fn locate(&self, path: &DocPath, prefer_key: bool) -> Option<SourceSpan> {
        if prefer_key && let Some(span) = self.key(path) {
            return Some(span);
        }
        let mut current = Some(path.clone());
        while let Some(p) = current {
            if let Some(span) = self.value(&p).or_else(|| self.key(&p)) {
                return Some(span);
            }
            current = p.parent();
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(feature = "yaml")]
mod spanned {
    //! A span-aware parse with `serde_saphyr::Spanned`. JSON is parsed by the
    //! same parser (JSON documents are YAML 1.2 flow documents).

    use std::fmt;

    use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use serde_saphyr::{Location, Spanned};

    use super::{DocPath, SourceMap, SourceSpan};

    /// A document tree that keeps the location of every node.
    enum Tree {
        Scalar,
        Seq(Vec<Spanned<Tree>>),
        Map(Vec<(Spanned<Key>, Spanned<Tree>)>),
    }

    /// A mapping key rendered as text.
    struct Key(String);

    pub(super) fn parse(text: &str) -> Option<SourceMap> {
        let options = serde_saphyr::options! { strict_booleans: true };
        let tree: Spanned<Tree> = serde_saphyr::from_str_with_options(text, options).ok()?;
        let mut map = SourceMap::default();
        record(&tree, &DocPath::root(), &mut map);
        Some(map)
    }

    fn span(location: &Location) -> Option<SourceSpan> {
        let line = u32::try_from(location.line()).ok().filter(|l| *l > 0)?;
        let column = u32::try_from(location.column()).ok().filter(|c| *c > 0)?;
        let len = u32::try_from(location.span().len()).unwrap_or(u32::MAX).max(1);
        Some(SourceSpan { line, column, len })
    }

    fn record(node: &Spanned<Tree>, path: &DocPath, map: &mut SourceMap) {
        if let Some(s) = span(&node.referenced) {
            map.values.insert(path.clone(), s);
        }
        match &node.value {
            Tree::Scalar => {}
            Tree::Seq(items) => {
                for (ix, item) in items.iter().enumerate() {
                    record(item, &path.index(ix), map);
                }
            }
            Tree::Map(entries) => {
                for (key, value) in entries {
                    let child = path.key(&key.value.0);
                    if let Some(s) = span(&key.referenced) {
                        map.keys.insert(child.clone(), s);
                    }
                    record(value, &child, map);
                }
            }
        }
    }

    struct TreeVisitor;

    impl<'de> Visitor<'de> for TreeVisitor {
        type Value = Tree;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("any YAML value")
        }

        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_i128<E: de::Error>(self, _: i128) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_u128<E: de::Error>(self, _: u128) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_none<E: de::Error>(self) -> Result<Tree, E> {
            Ok(Tree::Scalar)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Tree, D::Error> {
            Tree::deserialize(d)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Tree, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<Spanned<Tree>>()? {
                items.push(item);
            }
            Ok(Tree::Seq(items))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Tree, A::Error> {
            let mut entries = Vec::new();
            while let Some(key) = access.next_key::<Spanned<Key>>()? {
                let value = access.next_value::<Spanned<Tree>>()?;
                entries.push((key, value));
            }
            Ok(Tree::Map(entries))
        }
    }

    impl<'de> Deserialize<'de> for Tree {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(TreeVisitor)
        }
    }

    struct KeyVisitor;

    impl Visitor<'_> for KeyVisitor {
        type Value = Key;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a scalar mapping key")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Key, E> {
            Ok(Key(v.to_owned()))
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Key, E> {
            Ok(Key(v.to_string()))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Key, E> {
            Ok(Key(v.to_string()))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Key, E> {
            Ok(Key(v.to_string()))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Key, E> {
            Ok(Key(v.to_string()))
        }
        fn visit_unit<E: de::Error>(self) -> Result<Key, E> {
            Ok(Key("null".to_owned()))
        }
    }

    impl<'de> Deserialize<'de> for Key {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(KeyVisitor)
        }
    }
}

#[cfg(all(test, feature = "yaml"))]
mod tests {
    use super::*;

    #[test]
    fn records_key_and_value_positions() {
        let text = "graph:\n  nodes:\n    - id: a\n      templat: hi\n";
        let map = SourceMap::from_text(text);
        let path = DocPath::root().key("graph").key("nodes").index(0).key("templat");
        assert_eq!(map.key(&path), Some(SourceSpan { line: 4, column: 7, len: 7 }));
        assert_eq!(map.value(&path).map(|s| (s.line, s.column)), Some((4, 16)));
        assert_eq!(path.to_string(), "graph.nodes[0].templat");
    }

    #[test]
    fn json_positions() {
        let text = "{\"graph\": {\"nodes\": [\n  {\"id\": \"a\", \"type\": \"start\"}\n]}}";
        let map = SourceMap::from_text(text);
        let path = DocPath::root().key("graph").key("nodes").index(0).key("type");
        assert_eq!(map.key(&path).map(|s| (s.line, s.column)), Some((2, 15)));
    }

    #[test]
    fn locate_falls_back_to_ancestors() {
        let map = SourceMap::from_text("graph:\n  nodes: []\n");
        let path = DocPath::root().key("graph").key("nodes").index(3).key("x");
        assert_eq!(map.locate(&path, true).map(|s| s.line), Some(2));
        assert_eq!(DocPath::root().key("a b").index(1).to_string(), "[\"a b\"][1]");
    }
}
