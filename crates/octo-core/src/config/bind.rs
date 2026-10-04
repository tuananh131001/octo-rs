//! Binds a [`ConfigNode`] onto a settings struct, the way ConfigurationBinder does.
//!
//! The settings structs derive `Deserialize` with `#[serde(default)]`, and their `Default`
//! carries the C# property initializers. This deserializer then reproduces the binder's
//! rules:
//!
//! - a key matches a field ignoring case and underscores (`DownloadOnStar` →
//!   `download_on_star`), and keys that match nothing are ignored;
//! - every value is a string, parsed on demand into the field's type (`true`/`True`, `12`,
//!   `1.5`); enums match a variant name ignoring case, or a variant's position by number;
//! - an empty string leaves an optional field unset and a typed field at its default;
//! - list items are the children `0`, `1`, ... in numeric order.
//!
//! Where .NET would throw on a value it cannot convert, which fails the app at the first
//! settings read, this binder keeps the default for that field and records a warning, so a
//! typo in one setting does not take the whole server down.

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

use super::tree::ConfigNode;

/// A value that could not be converted, with the path it was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindWarning {
    pub path: String,
    pub message: String,
}

/// Binds `node` onto `T`, starting from `T::default()` for anything absent. Never fails:
/// conversion problems come back as warnings and leave defaults in place.
pub fn bind<T: DeserializeOwned + Default>(node: &ConfigNode) -> (T, Vec<BindWarning>) {
    // serde's derived visitors stop at the first bad field, so a bad value is removed and
    // the bind retried, which leaves that field at its default and keeps its neighbours.
    let mut node = node.clone();
    let mut warnings = Vec::new();
    for _ in 0..256 {
        let de = NodeDeserializer {
            node: &node,
            path: "",
        };
        match T::deserialize(de) {
            Ok(v) => return (v, warnings),
            Err(BindError {
                path: Some(path),
                message,
            }) => {
                if !message.is_empty() {
                    warnings.push(BindWarning {
                        path: path.clone(),
                        message,
                    });
                }
                if !remove_path(&mut node, &path) {
                    break;
                }
            }
            Err(e) => {
                warnings.push(BindWarning {
                    path: String::new(),
                    message: e.to_string(),
                });
                break;
            }
        }
    }
    (T::default(), warnings)
}

fn remove_path(node: &mut ConfigNode, path: &str) -> bool {
    let (head, rest) = match path.split_once(':') {
        Some((h, r)) => (h, Some(r)),
        None => (path, None),
    };
    let Some(idx) = node
        .children
        .iter()
        .position(|(k, _)| k.eq_ignore_ascii_case(head))
    else {
        return false;
    };
    match rest {
        None => {
            node.children.remove(idx);
            true
        }
        Some(r) => remove_path(&mut node.children[idx].1, r),
    }
}

#[derive(Debug)]
pub struct BindError {
    /// The path of the value that did not convert, when one is to blame.
    path: Option<String>,
    message: String,
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{p}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for BindError {}

impl de::Error for BindError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        BindError {
            path: None,
            message: msg.to_string(),
        }
    }
}

/// Normalises a key or field name for matching: lowercase, without underscores.
fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[derive(Clone, Copy)]
struct NodeDeserializer<'a> {
    node: &'a ConfigNode,
    path: &'a str,
}

// `path` is borrowed for the struct's lifetime; child paths are built as owned strings and
// leaked into short-lived boxes would be wasteful, so child deserializers own theirs.
struct OwnedNodeDeserializer<'a> {
    node: &'a ConfigNode,
    path: String,
}

impl<'a> NodeDeserializer<'a> {
    fn child(&self, key: &str, node: &'a ConfigNode) -> OwnedNodeDeserializer<'a> {
        let path = if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}:{}", self.path, key)
        };
        OwnedNodeDeserializer { node, path }
    }

    fn text(&self) -> Option<&'a str> {
        self.node.value.as_deref()
    }

    /// The values a typed setting may bind from, in order: the effective one when it is not
    /// empty, then what lower layers said. The binder takes the first that converts.
    fn candidates(&self) -> impl Iterator<Item = &'a str> {
        self.node
            .value
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .into_iter()
            .chain(self.node.fallbacks.iter().map(String::as_str))
            .map(str::trim)
    }

    /// The first candidate `parse` accepts. When none does, a rejection naming the
    /// effective value (empty when there was nothing to convert).
    fn first_parsed<T>(&self, parse: impl Fn(&str) -> Option<T>, what: &str) -> Result<T, BindError> {
        let mut first_bad: Option<&str> = None;
        for c in self.candidates() {
            match parse(c) {
                Some(v) => return Ok(v),
                None => {
                    first_bad.get_or_insert(c);
                }
            }
        }
        Err(self.reject(match first_bad {
            Some(t) => format!("'{t}' is not a valid {what}"),
            None => String::new(),
        }))
    }

    /// A value that does not convert: the bind drops it and retries. An empty message
    /// means "absent", which is not worth a warning.
    fn reject(&self, message: String) -> BindError {
        BindError {
            path: Some(self.path.to_string()),
            message,
        }
    }
}

impl<'a> OwnedNodeDeserializer<'a> {
    fn borrow(&self) -> NodeDeserializer<'_> {
        NodeDeserializer {
            node: self.node,
            path: &self.path,
        }
    }
}

/// Parses an integer or float the way Int32Converter and DoubleConverter do under the
/// invariant culture: surrounding whitespace and a sign are fine, and integers also take a
/// `0x`, `&h` or `#` hex prefix.
fn parse_dotnet_number<T: std::str::FromStr + TryFrom<i64>>(text: &str) -> Option<T> {
    let t = text.trim();
    if let Ok(v) = t.parse::<T>() {
        return Some(v);
    }
    let lower = t.to_ascii_lowercase();
    let hex = lower
        .strip_prefix("0x")
        .or_else(|| lower.strip_prefix("&h"))
        .or_else(|| lower.strip_prefix('#'))?;
    i64::from_str_radix(hex, 16)
        .ok()
        .and_then(|v| T::try_from(v).ok())
}

macro_rules! parse_number {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
            let n = self.first_parsed(|t| parse_dotnet_number::<$ty>(t), stringify!($ty))?;
            visitor.$visit(n)
        }
    };
}

macro_rules! parse_float {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
            let n = self.first_parsed(|t| t.trim().parse::<$ty>().ok(), stringify!($ty))?;
            visitor.$visit(n)
        }
    };
}

impl<'de, 'a> de::Deserializer<'de> for NodeDeserializer<'a> {
    type Error = BindError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        if !self.node.children.is_empty() {
            self.deserialize_map(visitor)
        } else {
            match self.text() {
                Some(t) => visitor.visit_str(t),
                None => visitor.visit_none(),
            }
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        let b = self.first_parsed(
            |t| {
                if t.eq_ignore_ascii_case("true") {
                    Some(true)
                } else if t.eq_ignore_ascii_case("false") {
                    Some(false)
                } else {
                    None
                }
            },
            "Boolean",
        )?;
        visitor.visit_bool(b)
    }

    parse_number!(deserialize_i8, visit_i8, i8);
    parse_number!(deserialize_i16, visit_i16, i16);
    parse_number!(deserialize_i32, visit_i32, i32);
    parse_number!(deserialize_i64, visit_i64, i64);
    parse_number!(deserialize_u8, visit_u8, u8);
    parse_number!(deserialize_u16, visit_u16, u16);
    parse_number!(deserialize_u32, visit_u32, u32);
    parse_number!(deserialize_u64, visit_u64, u64);
    parse_float!(deserialize_f32, visit_f32, f32);
    parse_float!(deserialize_f64, visit_f64, f64);

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        match self.text().and_then(|t| t.chars().next()) {
            Some(c) => visitor.visit_char(c),
            None => Err(self.reject(String::new())),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        match self.text() {
            Some(t) => visitor.visit_str(t),
            None if self.node.children.is_empty() => Err(self.reject(String::new())),
            None => Err(self.reject("a section cannot be read as a string".into())),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        // An empty string is "not set" for an optional field of any type; a section with
        // children (an optional nested object) is present.
        let empty_value = self.text().is_none_or(|t| t.is_empty());
        if empty_value && self.node.children.is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, BindError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, BindError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        let items: Vec<OwnedNodeDeserializer<'a>> = self
            .node
            .ordered_children()
            .into_iter()
            .map(|(k, n)| self.child(k, n))
            .collect();
        visitor.visit_seq(NodeSeq {
            items: items.into_iter(),
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, BindError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, BindError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        let entries: Vec<(String, OwnedNodeDeserializer<'a>)> = self
            .node
            .children
            .iter()
            .map(|(k, n)| (k.clone(), self.child(k, n)))
            .collect();
        visitor.visit_map(NodeMap {
            entries: entries.into_iter(),
            pending: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, BindError> {
        let wanted: Vec<(String, &'static str)> = fields.iter().map(|f| (normalise(f), *f)).collect();
        let mut entries = Vec::new();
        for (key, node) in &self.node.children {
            let k = normalise(key);
            if let Some((_, field)) = wanted.iter().find(|(n, _)| *n == k) {
                // A later spelling of the same field (env and file differing in case) has
                // already been merged by ConfigTree; the first wins here.
                if entries.iter().any(|(f, _): &(String, _)| f == field) {
                    continue;
                }
                entries.push((field.to_string(), self.child(key, node)));
            }
        }
        visitor.visit_map(NodeMap {
            entries: entries.into_iter(),
            pending: None,
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, BindError> {
        let found = self.first_parsed(
            |text| {
                variants
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(text) || normalise(v) == normalise(text))
                    .copied()
                    .or_else(|| text.parse::<usize>().ok().and_then(|i| variants.get(i).copied()))
            },
            &format!("value (one of {})", variants.join(", ")),
        )?;
        visitor.visit_enum(found.into_deserializer())
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, BindError> {
        visitor.visit_unit()
    }
}

struct NodeSeq<'a, I: Iterator<Item = OwnedNodeDeserializer<'a>>> {
    items: I,
}

impl<'de, 'a, I: Iterator<Item = OwnedNodeDeserializer<'a>>> SeqAccess<'de> for NodeSeq<'a, I> {
    type Error = BindError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, BindError> {
        // A list item that does not convert is removed by the retry in `bind`.
        match self.items.next() {
            Some(item) => seed.deserialize(item.borrow()).map(Some),
            None => Ok(None),
        }
    }
}

struct NodeMap<'a, I: Iterator<Item = (String, OwnedNodeDeserializer<'a>)>> {
    entries: I,
    pending: Option<OwnedNodeDeserializer<'a>>,
}

impl<'de, 'a, I: Iterator<Item = (String, OwnedNodeDeserializer<'a>)>> MapAccess<'de> for NodeMap<'a, I> {
    type Error = BindError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, BindError> {
        match self.entries.next() {
            Some((key, value)) => {
                self.pending = Some(value);
                seed.deserialize(key.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, BindError> {
        let value = self.pending.take().expect("value follows its key");
        seed.deserialize(value.borrow())
    }
}

/// Deserializes `T` from a node, where a conversion failure anywhere is an error rather than
/// a default. For callers that bind one value, not a whole settings class.
pub fn bind_strict<T: DeserializeOwned>(node: &ConfigNode) -> Result<T, String> {
    T::deserialize(NodeDeserializer { node, path: "" }).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tree::ConfigTree;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
    enum Mode {
        #[default]
        Stream,
        Permanent,
        Cache,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(default)]
    struct Sample {
        url: Option<String>,
        download_on_star: bool,
        search_wait_seconds: i32,
        ratio: f64,
        storage_mode: Mode,
        clients: Vec<String>,
        nested: Inner,
    }

    #[derive(Debug, Deserialize, PartialEq, Default)]
    #[serde(default)]
    struct Inner {
        name: String,
    }

    impl Default for Sample {
        fn default() -> Self {
            Self {
                url: None,
                download_on_star: true,
                search_wait_seconds: 30,
                ratio: 0.5,
                storage_mode: Mode::Stream,
                clients: vec![],
                nested: Inner::default(),
            }
        }
    }

    fn bind_env(vars: &[(&str, &str)]) -> (Sample, Vec<BindWarning>) {
        let tree = ConfigTree::from_env_vars(vars.iter().map(|(k, v)| (*k, *v)));
        bind::<Sample>(&tree.section("S"))
    }

    #[test]
    fn absent_keys_keep_the_initializer_defaults() {
        let (s, w) = bind_env(&[]);
        assert_eq!(s, Sample::default());
        assert!(w.is_empty());
    }

    #[test]
    fn keys_match_fields_ignoring_case_and_values_parse_from_strings() {
        let (s, w) = bind_env(&[
            ("S__URL", "http://x"),
            ("s__DownloadOnStar", "False"),
            ("S__searchWaitSeconds", " 45 "),
            ("S__Ratio", "0.25"),
            ("S__StorageMode", "permanent"),
            ("S__Clients__1", "b"),
            ("S__Clients__0", "a"),
            ("S__Nested__Name", "n"),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s.url.as_deref(), Some("http://x"));
        assert!(!s.download_on_star);
        assert_eq!(s.search_wait_seconds, 45);
        assert_eq!(s.ratio, 0.25);
        assert_eq!(s.storage_mode, Mode::Permanent);
        assert_eq!(s.clients, ["a", "b"]);
        assert_eq!(s.nested.name, "n");
    }

    #[test]
    fn enums_also_bind_by_number() {
        let (s, _) = bind_env(&[("S__StorageMode", "2")]);
        assert_eq!(s.storage_mode, Mode::Cache);
    }

    #[test]
    fn a_bad_value_keeps_its_default_and_its_neighbours() {
        let (s, w) = bind_env(&[
            ("S__SearchWaitSeconds", "soon"),
            ("S__DownloadOnStar", "false"),
            ("S__StorageMode", "Sometimes"),
        ]);
        assert_eq!(s.search_wait_seconds, 30);
        assert_eq!(s.storage_mode, Mode::Stream);
        assert!(!s.download_on_star);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].path, "SearchWaitSeconds");
    }

    #[test]
    fn an_empty_or_bad_top_value_falls_back_to_the_layer_below() {
        let mut tree =
            ConfigTree::from_env_vars([("S__SearchWaitSeconds", "45"), ("S__DownloadOnStar", "false")]);
        tree.merge(&ConfigTree::from_json(
            &serde_json::json!({"S": {"SearchWaitSeconds": null, "DownloadOnStar": "maybe"}}),
        ));
        let (s, w) = bind::<Sample>(&tree.section("S"));
        assert_eq!(s.search_wait_seconds, 45);
        assert!(!s.download_on_star);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn integers_take_dotnet_hex_prefixes() {
        let (s, _) = bind_env(&[("S__SearchWaitSeconds", "0x1E")]);
        assert_eq!(s.search_wait_seconds, 30);
    }

    #[test]
    fn empty_strings_mean_unset() {
        let (s, w) = bind_env(&[("S__Url", ""), ("S__SearchWaitSeconds", "")]);
        assert_eq!(s.url, None);
        assert_eq!(s.search_wait_seconds, 30);
        assert!(w.is_empty());
    }
}
