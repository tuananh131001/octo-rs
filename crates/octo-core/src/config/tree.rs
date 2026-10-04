//! A flat, case-insensitive key/value view of layered configuration, the way
//! Microsoft.Extensions.Configuration sees it.
//!
//! Every source (appsettings.json, appsettings.{Environment}.json, environment variables,
//! /app/config/settings.json) is flattened into `Section:Key` paths with string values, and
//! a later source replaces an earlier one key by key. Keys compare ignoring ASCII case, as
//! they do in .NET, so `SUBSONIC__URL`, `Subsonic__Url` and `"subsonic": {"url": ...}` all
//! land on the same setting.

use indexmap::IndexMap;
use serde_json::Value;

/// The key separator .NET uses for configuration paths.
pub const KEY_DELIMITER: &str = ":";

/// One layer, or the merge of several: path → value, in insertion order.
///
/// The map key is the lowercased path; the entry keeps the path as first spelled so error
/// messages and the settings writer can show it as written. Each entry also keeps the
/// values lower layers gave the same path, so a typed setting the top layer leaves empty
/// (the dashboard saves a cleared number field as null) can fall back to them; see
/// [`ConfigNode::fallbacks`].
#[derive(Debug, Clone, Default)]
pub struct ConfigTree {
    entries: IndexMap<String, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    path: String,
    /// Values from the lowest layer to the highest; the last is the effective one.
    layers: Vec<Option<String>>,
}

impl Entry {
    fn top(&self) -> Option<&str> {
        self.layers.last().and_then(|v| v.as_deref())
    }
}

impl ConfigTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets one path. A later `set` of the same path, in any case, replaces the value but
    /// keeps the first spelling, as .NET's provider dictionary does.
    pub fn set(&mut self, path: &str, value: Option<String>) {
        let key = path.to_ascii_lowercase();
        match self.entries.get_mut(&key) {
            Some(entry) => match entry.layers.last_mut() {
                Some(top) => *top = value,
                None => entry.layers.push(value),
            },
            None => {
                self.entries.insert(
                    key,
                    Entry {
                        path: path.to_string(),
                        layers: vec![value],
                    },
                );
            }
        }
    }

    /// The value at a path, ignoring case. `None` when absent or set to null.
    pub fn get(&self, path: &str) -> Option<&str> {
        self.entries.get(&path.to_ascii_lowercase()).and_then(Entry::top)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(&path.to_ascii_lowercase())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every (path, effective value) pair in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.entries.values().map(|e| (e.path.as_str(), e.top()))
    }

    /// Layers `other` over this tree: each of its keys takes precedence over ours, and ours
    /// stay underneath as fallbacks.
    pub fn merge(&mut self, other: &ConfigTree) {
        for (key, theirs) in &other.entries {
            match self.entries.get_mut(key) {
                Some(ours) => ours.layers.extend(theirs.layers.iter().cloned()),
                None => {
                    self.entries.insert(key.clone(), theirs.clone());
                }
            }
        }
    }

    /// Flattens a JSON document the way JsonConfigurationProvider does: objects become
    /// `a:b` paths, array items become `a:0`, `a:1`, and every scalar becomes its string
    /// form (`true`, `12`, `1.5`). A null, and an empty object or array, become a null
    /// value at that path.
    pub fn from_json(value: &Value) -> Self {
        let mut tree = ConfigTree::new();
        flatten_json(&mut tree, "", value);
        tree
    }

    /// Reads a JSON settings file. A missing file is an empty layer; a file that does not
    /// parse is an error, which callers report and treat as an empty layer, as an optional
    /// .NET JSON source does after logging.
    pub fn from_json_file(path: &std::path::Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                // .NET accepts a UTF-8 BOM and trailing commas and comments in config JSON.
                let text = String::from_utf8_lossy(&bytes);
                let text = text.trim_start_matches('\u{feff}');
                let value = parse_lenient_json(text)?;
                Ok(Self::from_json(&value))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigTree::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Environment variables, with `__` standing for the `:` separator, as
    /// EnvironmentVariablesConfigurationProvider maps them. Every variable is included, with
    /// no prefix filter, as WebApplication.CreateBuilder adds them.
    pub fn from_env_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut tree = ConfigTree::new();
        for (key, value) in vars {
            let path = key.as_ref().replace("__", KEY_DELIMITER);
            tree.set(&path, Some(value.into()));
        }
        tree
    }

    /// The configuration below `section` as a node tree for binding.
    pub fn section(&self, section: &str) -> ConfigNode {
        let mut root = ConfigNode::default();
        let prefix = format!("{}{}", section.to_ascii_lowercase(), KEY_DELIMITER);
        for (key, entry) in &self.entries {
            if key == &section.to_ascii_lowercase() {
                root.set_from(entry);
                continue;
            }
            if let Some(rest) = key.strip_prefix(&prefix) {
                // Keep the original spelling of the remaining segments.
                let original_rest = &entry.path[entry.path.len() - rest.len()..];
                root.insert(original_rest, entry);
            }
        }
        root
    }

    /// The whole configuration as a node tree.
    pub fn root(&self) -> ConfigNode {
        let mut root = ConfigNode::default();
        for entry in self.entries.values() {
            root.insert(&entry.path.clone(), entry);
        }
        root
    }
}

fn flatten_json(tree: &mut ConfigTree, prefix: &str, value: &Value) {
    let child_path = |segment: &str| {
        if prefix.is_empty() {
            segment.to_string()
        } else {
            format!("{prefix}{KEY_DELIMITER}{segment}")
        }
    };
    match value {
        Value::Object(map) => {
            if map.is_empty() && !prefix.is_empty() {
                tree.set(prefix, None);
            }
            for (k, v) in map {
                flatten_json(tree, &child_path(k), v);
            }
        }
        Value::Array(items) => {
            if items.is_empty() && !prefix.is_empty() {
                tree.set(prefix, None);
            }
            for (i, v) in items.iter().enumerate() {
                flatten_json(tree, &child_path(&i.to_string()), v);
            }
        }
        Value::Null => tree.set(prefix, Some(String::new())),
        Value::Bool(b) => tree.set(prefix, Some(if *b { "True" } else { "False" }.to_string())),
        Value::Number(n) => tree.set(prefix, Some(n.to_string())),
        Value::String(s) => tree.set(prefix, Some(s.clone())),
    }
}

/// Parses JSON that may carry `//` and `/* */` comments and trailing commas, which the .NET
/// configuration reader allows.
pub fn parse_lenient_json(text: &str) -> anyhow::Result<Value> {
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(_) => Ok(serde_json::from_str(&strip_comments_and_trailing_commas(text))?),
    }
}

fn strip_comments_and_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == ',' {
            // Drop the comma when only whitespace separates it from a closing bracket.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// A configuration section: an optional value of its own plus named children, as
/// IConfigurationSection exposes them.
#[derive(Debug, Clone, Default)]
pub struct ConfigNode {
    pub value: Option<String>,
    /// Non-empty values lower layers gave this path, highest first. A typed setting whose
    /// effective value is empty or does not convert binds the first of these that does.
    pub fallbacks: Vec<String>,
    /// Children in first-seen order, keyed by the segment as first spelled.
    pub children: Vec<(String, ConfigNode)>,
}

impl ConfigNode {
    /// A leaf with a single value and no fallbacks.
    pub fn leaf(value: impl Into<String>) -> Self {
        ConfigNode {
            value: Some(value.into()),
            ..Default::default()
        }
    }

    fn set_from(&mut self, entry: &Entry) {
        self.value = entry.top().map(str::to_string);
        let n = entry.layers.len();
        self.fallbacks = entry.layers[..n.saturating_sub(1)]
            .iter()
            .rev()
            .filter_map(|v| v.as_deref())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect();
    }

    fn insert(&mut self, path: &str, entry: &Entry) {
        let (head, rest) = match path.split_once(KEY_DELIMITER) {
            Some((h, r)) => (h, Some(r)),
            None => (path, None),
        };
        let idx = match self
            .children
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(head))
        {
            Some(i) => i,
            None => {
                self.children.push((head.to_string(), ConfigNode::default()));
                self.children.len() - 1
            }
        };
        let child = &mut self.children[idx].1;
        match rest {
            Some(r) => child.insert(r, entry),
            None => child.set_from(entry),
        }
    }

    /// A child section by name, ignoring case.
    pub fn child(&self, name: &str) -> Option<&ConfigNode> {
        self.children
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// Whether the section holds anything: a non-null value or any child.
    pub fn exists(&self) -> bool {
        self.value.is_some() || !self.children.is_empty()
    }

    /// Children in the order IConfiguration.GetChildren returns them: keys that are
    /// integers first in numeric order, then the rest ordinally ignoring case. This is the
    /// order list items bind in.
    pub fn ordered_children(&self) -> Vec<(&str, &ConfigNode)> {
        let mut items: Vec<(&str, &ConfigNode)> =
            self.children.iter().map(|(k, v)| (k.as_str(), v)).collect();
        items.sort_by(|(a, _), (b, _)| match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
        });
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn env_vars_map_double_underscore_to_sections_ignoring_case() {
        let tree = ConfigTree::from_env_vars([("SUBSONIC__URL", "http://nd:4533"), ("Other", "x")]);
        assert_eq!(tree.get("Subsonic:Url"), Some("http://nd:4533"));
        assert_eq!(tree.get("subsonic:url"), Some("http://nd:4533"));
        assert_eq!(tree.get("other"), Some("x"));
    }

    #[test]
    fn later_layers_replace_earlier_ones_key_by_key() {
        let mut base = ConfigTree::from_json(&json!({"Subsonic": {"Url": "a", "StorageMode": "Stream"}}));
        let over = ConfigTree::from_env_vars([("subsonic__url", "b")]);
        base.merge(&over);
        assert_eq!(base.get("Subsonic:Url"), Some("b"));
        assert_eq!(base.get("Subsonic:StorageMode"), Some("Stream"));
    }

    #[test]
    fn json_arrays_become_indexed_keys_and_bools_their_dotnet_spelling() {
        let tree = ConfigTree::from_json(&json!({"A": {"List": ["x", "y"], "On": true, "N": 1.5}}));
        assert_eq!(tree.get("A:List:0"), Some("x"));
        assert_eq!(tree.get("A:List:1"), Some("y"));
        assert_eq!(tree.get("A:On"), Some("True"));
        assert_eq!(tree.get("A:N"), Some("1.5"));
    }

    #[test]
    fn lower_layers_stay_reachable_as_fallbacks() {
        let mut tree = ConfigTree::from_json(&json!({"S": {"N": 5}}));
        tree.merge(&ConfigTree::from_env_vars([("S__N", "7")]));
        tree.merge(&ConfigTree::from_json(&json!({"S": {"N": null}})));
        let node = tree.section("S");
        let n = node.child("n").unwrap();
        assert_eq!(n.value.as_deref(), Some(""));
        assert_eq!(n.fallbacks, ["7", "5"]);
    }

    #[test]
    fn children_order_numeric_keys_numerically() {
        let tree = ConfigTree::from_env_vars([("L__10", "c"), ("L__2", "b"), ("L__0", "a")]);
        let node = tree.section("L");
        let keys: Vec<&str> = node.ordered_children().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["0", "2", "10"]);
    }

    #[test]
    fn lenient_json_allows_comments_and_trailing_commas() {
        let v =
            parse_lenient_json("{ // note\n \"a\": [1, 2,], /* x */ \"b\": \"//not a comment\", }").unwrap();
        assert_eq!(v, json!({"a": [1, 2], "b": "//not a comment"}));
    }
}
