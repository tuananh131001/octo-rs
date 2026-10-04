//! A small stand-in for `System.Xml.Linq`'s `XElement`, writing exactly what
//! `XDocument.ToString()` writes.
//!
//! Octo built every Subsonic XML answer as an `XDocument` and sent `doc.ToString()`. Clients
//! parse it, but the parity harness compares bytes, so the writer reproduces .NET's layout:
//! two-space indentation with `\n` line breaks, no XML declaration, `<name />` for an element
//! with no content and `<name></name>` for one holding an empty string, the namespace
//! declaration after the element's own attributes, attribute values with `<`, `>`, `&`, `"`
//! and line breaks escaped (but not `'`), text with `<`, `>` and `&` escaped and its line breaks
//! normalised to `\n`, and an element holding text written inline.

use chrono::{DateTime, FixedOffset, Utc};

/// One node of element content.
#[derive(Debug, Clone, PartialEq)]
pub enum XNode {
    Element(XElement),
    Text(String),
}

/// An element: a local name, attributes in insertion order, and content.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct XElement {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub content: Vec<XNode>,
    /// The default namespace, declared on this element when it differs from its parent's.
    pub namespace: Option<String>,
}

impl XElement {
    pub fn new(name: impl Into<String>) -> Self {
        XElement {
            name: name.into(),
            ..Default::default()
        }
    }

    /// An element in a namespace, as `new XElement(ns + name)`.
    pub fn ns(namespace: &str, name: impl Into<String>) -> Self {
        XElement {
            name: name.into(),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        }
    }

    /// Adds an attribute. `XAttribute` takes any value and writes it through `XmlConvert`;
    /// see [`XValue`] for the conversions.
    pub fn attr(mut self, name: impl Into<String>, value: impl XValue) -> Self {
        self.set_attr(name, value);
        self
    }

    pub fn set_attr(&mut self, name: impl Into<String>, value: impl XValue) {
        let name = name.into();
        let value = value.to_xml_value();
        // XElement.SetAttributeValue replaces in place; Add of a duplicate would throw, and
        // no Octo caller relies on that.
        if let Some(slot) = self.attributes.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else {
            self.attributes.push((name, value));
        }
    }

    /// Adds an attribute only when there is a value, the `if (x != null) el.Add(...)` idiom.
    pub fn attr_opt<V: XValue>(self, name: impl Into<String>, value: Option<V>) -> Self {
        match value {
            Some(v) => self.attr(name, v),
            None => self,
        }
    }

    pub fn child(mut self, child: XElement) -> Self {
        self.content.push(XNode::Element(child));
        self
    }

    pub fn push(&mut self, child: XElement) {
        self.content.push(XNode::Element(child));
    }

    pub fn children(mut self, children: impl IntoIterator<Item = XElement>) -> Self {
        self.content.extend(children.into_iter().map(XNode::Element));
        self
    }

    /// Text content, as `new XElement(name, "text")`. An empty string still makes the
    /// element non-empty: it is written `<name></name>`.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.content.push(XNode::Text(text.into()));
        self
    }

    /// The element and its descendants as `XElement.ToString()` writes them.
    pub fn to_xml_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0, None, true);
        out
    }

    fn write(&self, out: &mut String, depth: usize, parent_ns: Option<&str>, indent: bool) {
        out.push('<');
        out.push_str(&self.name);
        for (k, v) in &self.attributes {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            escape_attribute(out, v);
            out.push('"');
        }
        if let Some(ns) = self.namespace.as_deref()
            && parent_ns != Some(ns)
        {
            out.push_str(" xmlns=\"");
            escape_attribute(out, ns);
            out.push('"');
        }
        if self.content.is_empty() {
            out.push_str(" />");
            return;
        }
        out.push('>');
        let ns = self.namespace.as_deref().or(parent_ns);
        // XmlWriter stops indenting inside an element once it holds text: mixed content is
        // written as it stands.
        let mixed = self.content.iter().any(|c| matches!(c, XNode::Text(_)));
        let child_indent = indent && !mixed;
        for node in &self.content {
            match node {
                XNode::Text(t) => escape_text(out, t),
                XNode::Element(e) => {
                    if child_indent {
                        newline(out, depth + 1);
                    }
                    e.write(out, depth + 1, ns, child_indent);
                }
            }
        }
        if child_indent {
            newline(out, depth);
        }
        out.push_str("</");
        out.push_str(&self.name);
        out.push('>');
    }
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn escape_attribute(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            '\t' => out.push_str("&#x9;"),
            _ => out.push(c),
        }
    }
}

fn escape_text(out: &mut String, value: &str) {
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            // NewLineHandling.Replace: \r\n and a lone \r both become the writer's \n.
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            _ => out.push(c),
        }
    }
}

/// A value an attribute can take, converted the way `XAttribute`'s constructor does through
/// `XmlConvert.ToString`.
pub trait XValue {
    fn to_xml_value(&self) -> String;
}

impl XValue for &str {
    fn to_xml_value(&self) -> String {
        (*self).to_string()
    }
}

impl XValue for String {
    fn to_xml_value(&self) -> String {
        self.clone()
    }
}

impl XValue for &String {
    fn to_xml_value(&self) -> String {
        (*self).clone()
    }
}

impl XValue for bool {
    fn to_xml_value(&self) -> String {
        if *self { "true" } else { "false" }.to_string()
    }
}

macro_rules! int_value {
    ($($t:ty),*) => {$(
        impl XValue for $t {
            fn to_xml_value(&self) -> String {
                self.to_string()
            }
        }
    )*};
}

int_value!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);

impl XValue for f64 {
    fn to_xml_value(&self) -> String {
        octo_core::json::format_double(*self)
    }
}

impl XValue for f32 {
    fn to_xml_value(&self) -> String {
        octo_core::json::format_single(*self)
    }
}

/// `XmlConvert.ToString(DateTime, RoundtripKind)` for a UTC time: trimmed fraction and `Z`.
impl XValue for DateTime<Utc> {
    fn to_xml_value(&self) -> String {
        octo_core::json::datetime::format_utc(self)
    }
}

/// `XmlConvert.ToString(DateTimeOffset)`: `Z` for a zero offset, `+hh:mm` otherwise.
impl XValue for DateTime<FixedOffset> {
    fn to_xml_value(&self) -> String {
        if self.offset().local_minus_utc() == 0 {
            octo_core::json::datetime::format_utc(&self.with_timezone(&Utc))
        } else {
            octo_core::json::datetime::format_offset(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "http://subsonic.org/restapi";

    #[test]
    fn layout_and_escaping_match_xdocument_to_string() {
        // Captured from .NET 9: new XDocument(...).ToString() for the same tree.
        let doc = XElement::ns(NS, "subsonic-response")
            .attr("status", "ok")
            .attr("version", "1.16.1")
            .child(XElement::ns(NS, "ping"))
            .child(
                XElement::ns(NS, "song")
                    .attr("id", "a<b>&\"c'\n\t\r \u{e9} \u{1f6e0}")
                    .attr("duration", 12)
                    .attr("isDir", false)
                    .attr("bitRate", 1.5)
                    .child(XElement::ns(NS, "genre").attr("name", "Rock")),
            )
            .child(XElement::ns(NS, "lyrics").text("line1\nline2 <&> \" ' \u{e9}\r\n\tx"))
            .child(XElement::ns(NS, "empty").text(""))
            .child(
                XElement::ns(NS, "nested")
                    .child(XElement::ns(NS, "a").text("t"))
                    .child(XElement::ns(NS, "b")),
            );
        let want = concat!(
            "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <ping />\n",
            "  <song id=\"a&lt;b&gt;&amp;&quot;c'&#xA;&#x9;&#xD; \u{e9} \u{1f6e0}\" duration=\"12\" isDir=\"false\" bitRate=\"1.5\">\n",
            "    <genre name=\"Rock\" />\n",
            "  </song>\n",
            "  <lyrics>line1\nline2 &lt;&amp;&gt; \" ' \u{e9}\n\tx</lyrics>\n",
            "  <empty></empty>\n",
            "  <nested>\n",
            "    <a>t</a>\n",
            "    <b />\n",
            "  </nested>\n",
            "</subsonic-response>"
        );
        assert_eq!(doc.to_xml_string(), want);
    }

    #[test]
    fn values_convert_like_xmlconvert() {
        use chrono::TimeZone;
        let d = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let el = XElement::ns(NS, "x")
            .attr("d", d)
            .attr("o", d.fixed_offset())
            .attr("f", 0.1)
            .attr("big", 1e21)
            .attr("ff", 1.0f32 / 3.0);
        assert_eq!(
            el.to_xml_string(),
            "<x d=\"2026-01-02T03:04:05Z\" o=\"2026-01-02T03:04:05Z\" f=\"0.1\" big=\"1E+21\" ff=\"0.33333334\" xmlns=\"http://subsonic.org/restapi\" />"
        );
    }

    #[test]
    fn elements_without_a_namespace_declare_none() {
        let el = XElement::new("plain").child(XElement::new("child").text("x"));
        assert_eq!(el.to_xml_string(), "<plain>\n  <child>x</child>\n</plain>");
    }
}
