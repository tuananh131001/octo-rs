//! A JSON tree that keeps numbers as written, like System.Text.Json's `JsonNode`.
//!
//! settings.json is edited by hand and by the dashboard, and C#'s `JsonNode` writes a number
//! back with its original text (`1.50` stays `1.50`, `1e3` stays `1e3`). serde_json's
//! `Value` would normalise those, so the settings writer works on this tree instead. Parsing
//! is lenient the way the config reader is: comments and trailing commas are accepted (and
//! dropped on the next write).

use super::format::{Escaping, write_escaped};
use indexmap::IndexMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    /// The number's text exactly as it appeared.
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(IndexMap<String, Node>),
}

impl Node {
    pub fn object() -> Node {
        Node::Object(IndexMap::new())
    }

    pub fn as_object(&self) -> Option<&IndexMap<String, Node>> {
        match self {
            Node::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut IndexMap<String, Node>> {
        match self {
            Node::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Node::Object(_))
    }

    /// A property by exact name.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_object().and_then(|m| m.get(key))
    }

    /// A property by name ignoring ASCII case, with the name as stored.
    pub fn get_ignore_case(&self, key: &str) -> Option<(&str, &Node)> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(k, v)| (k.as_str(), v))
    }

    /// Parses JSON text, accepting comments and trailing commas.
    pub fn parse(text: &str) -> Result<Node, String> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        p.skip_ws()?;
        let node = p.value()?;
        p.skip_ws()?;
        if p.i != p.s.len() {
            return Err(format!("unexpected character at offset {}", p.i));
        }
        Ok(node)
    }

    /// Converts from a serde_json value. Numbers take serde_json's text for them.
    pub fn from_value(value: &serde_json::Value) -> Node {
        match value {
            serde_json::Value::Null => Node::Null,
            serde_json::Value::Bool(b) => Node::Bool(*b),
            serde_json::Value::Number(n) => Node::Number(n.to_string()),
            serde_json::Value::String(s) => Node::String(s.clone()),
            serde_json::Value::Array(a) => Node::Array(a.iter().map(Node::from_value).collect()),
            serde_json::Value::Object(m) => {
                Node::Object(m.iter().map(|(k, v)| (k.clone(), Node::from_value(v))).collect())
            }
        }
    }

    /// Converts to a serde_json value; a number that does not parse becomes null.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Node::Null => serde_json::Value::Null,
            Node::Bool(b) => serde_json::Value::Bool(*b),
            Node::Number(n) => serde_json::from_str(n).unwrap_or(serde_json::Value::Null),
            Node::String(s) => serde_json::Value::String(s.clone()),
            Node::Array(a) => serde_json::Value::Array(a.iter().map(Node::to_value).collect()),
            Node::Object(m) => {
                serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
            }
        }
    }

    /// Writes the tree as `JsonNode.ToJsonString` does: default encoder escaping, numbers
    /// verbatim, and two-space indentation when `indented`.
    pub fn to_json_string(&self, indented: bool) -> String {
        let mut out = String::new();
        self.write(&mut out, indented, 0);
        out
    }

    fn write(&self, out: &mut String, indented: bool, depth: usize) {
        let newline = |out: &mut String, depth: usize| {
            if indented {
                out.push('\n');
                for _ in 0..depth {
                    out.push_str("  ");
                }
            }
        };
        match self {
            Node::Null => out.push_str("null"),
            Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Node::Number(n) => out.push_str(n),
            Node::String(s) => write_string(out, s),
            Node::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    item.write(out, indented, depth + 1);
                }
                if !items.is_empty() {
                    newline(out, depth);
                }
                out.push(']');
            }
            Node::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    write_string(out, k);
                    out.push_str(if indented { ": " } else { ":" });
                    v.write(out, indented, depth + 1);
                }
                if !map.is_empty() {
                    newline(out, depth);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    let mut buf = Vec::with_capacity(s.len() + 2);
    buf.push(b'"');
    write_escaped(&mut buf, s, Escaping::Default).expect("writing to memory");
    buf.push(b'"');
    out.push_str(std::str::from_utf8(&buf).expect("escaped output is UTF-8"));
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_ws(&mut self) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(0xEF) if self.s[self.i..].starts_with(&[0xEF, 0xBB, 0xBF]) => self.i += 3,
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    loop {
                        match self.peek() {
                            None => return Err("unterminated comment".into()),
                            Some(b'*') if self.s.get(self.i + 1) == Some(&b'/') => {
                                self.i += 2;
                                break;
                            }
                            _ => self.i += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn expect(&mut self, lit: &str) -> Result<(), String> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(format!("expected '{lit}' at offset {}", self.i))
        }
    }

    fn value(&mut self) -> Result<Node, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Node::String(self.string()?)),
            Some(b't') => self.expect("true").map(|_| Node::Bool(true)),
            Some(b'f') => self.expect("false").map(|_| Node::Bool(false)),
            Some(b'n') => self.expect("null").map(|_| Node::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(format!("unexpected character at offset {}", self.i)),
            None => Err("unexpected end of input".into()),
        }
    }

    fn number(&mut self) -> Result<Node, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits_start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == digits_start {
            return Err(format!("invalid number at offset {start}"));
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            let frac = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == frac {
                return Err(format!("invalid number at offset {start}"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let exp = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == exp {
                return Err(format!("invalid number at offset {start}"));
            }
        }
        Ok(Node::Number(
            String::from_utf8_lossy(&self.s[start..self.i]).into_owned(),
        ))
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while matches!(self.peek(), Some(c) if c != b'"' && c != b'\\' && c >= 0x20) {
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?);
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.peek().ok_or("unterminated escape")?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) && self.s[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                let cp = 0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00);
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            } else {
                                out.push(char::from_u32(hi as u32).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return Err(format!("invalid escape at offset {}", self.i)),
                    }
                }
                Some(_) => return Err(format!("control character in string at offset {}", self.i)),
                None => return Err("unterminated string".into()),
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        let text = self.s.get(self.i..self.i + 4).ok_or("short \\u escape")?;
        let v = u16::from_str_radix(std::str::from_utf8(text).map_err(|e| e.to_string())?, 16)
            .map_err(|e| e.to_string())?;
        self.i += 4;
        Ok(v)
    }

    fn array(&mut self) -> Result<Node, String> {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws()?;
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(Node::Array(items));
            }
            items.push(self.value()?);
            self.skip_ws()?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at offset {}", self.i)),
            }
        }
    }

    fn object(&mut self) -> Result<Node, String> {
        self.i += 1;
        let mut map = IndexMap::new();
        loop {
            self.skip_ws()?;
            match self.peek() {
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Node::Object(map));
                }
                Some(b'"') => {}
                _ => return Err(format!("expected a property name at offset {}", self.i)),
            }
            let key = self.string()?;
            self.skip_ws()?;
            self.expect(":")?;
            self.skip_ws()?;
            let value = self.value()?;
            // JsonNode keeps the last of two equal names.
            map.insert(key, value);
            self.skip_ws()?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Node::Object(map));
                }
                _ => return Err(format!("expected ',' or '}}' at offset {}", self.i)),
            }
        }
    }
}

impl std::fmt::Display for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_json_string(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_text_and_layout_matches_json_node() {
        // Captured from .NET 9: JsonNode.Parse(...).ToJsonString(WriteIndented = true).
        let n = Node::parse(r#"{"a": 1.50, "b": [], "c": {}, "d": [1, {"x": null}], "e": "é", "f": 1e3}"#)
            .unwrap();
        assert_eq!(
            n.to_json_string(true),
            "{\n  \"a\": 1.50,\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    1,\n    {\n      \"x\": null\n    }\n  ],\n  \"e\": \"\\u00E9\",\n  \"f\": 1e3\n}"
        );
    }

    #[test]
    fn comments_and_trailing_commas_are_read_and_dropped() {
        let n = Node::parse("{ /* c */ \"a\": [1, 2,], // x\n }").unwrap();
        assert_eq!(n.to_json_string(false), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn escapes_and_surrogates_round_trip() {
        let n = Node::parse(r#""\uD83D\uDEE0 \u00e9 \"q\" \n""#).unwrap();
        assert_eq!(n, Node::String("🛠 é \"q\" \n".into()));
        assert_eq!(
            n.to_json_string(false),
            r#""\uD83D\uDEE0 \u00E9 \u0022q\u0022 \n""#
        );
    }
}
