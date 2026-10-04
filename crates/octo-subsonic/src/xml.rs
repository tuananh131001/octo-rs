//! A small stand-in for `System.Xml.Linq`'s `XElement`, writing exactly what
//! `XDocument.ToString()` writes, and reading what `XDocument.Parse` reads.
//!
//! Octo built every Subsonic XML answer as an `XDocument` and sent `doc.ToString()`. Clients
//! parse it, but the parity harness compares bytes, so the writer reproduces .NET's layout:
//! two-space indentation with `\n` line breaks, no XML declaration, `<name />` for an element
//! with no content and `<name></name>` for one holding an empty string, the namespace
//! declaration after the element's own attributes, attribute values with `<`, `>`, `&`, `"`
//! and line breaks escaped (but not `'`), text with `<`, `>` and `&` escaped and its line breaks
//! normalised to `\n`, and an element holding text written inline.
//!
//! Octo also re-serialised Navidrome's XML (the search merge, the OpenSubsonic extensions,
//! the sync catalog page), so [`XElement::parse`] reads a document the way
//! `XDocument.Parse(text)` (`LoadOptions.None`) does: whitespace-only text is dropped,
//! `<a></a>` stays distinct from `<a/>`, attribute values are normalised, and an `xmlns`
//! declaration is kept as an attribute in its place.

use std::borrow::Cow;

use chrono::{DateTime, FixedOffset, Utc};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// One node of element content.
#[derive(Debug, Clone, PartialEq)]
pub enum XNode {
    Element(XElement),
    Text(String),
    /// `XCData`: written as `<![CDATA[...]]>`, and like text it stops indentation.
    CData(String),
    /// `XComment`: written as `<!--...-->` on its own line, like an element.
    Comment(String),
}

/// An element: a local name, attributes in insertion order, and content.
///
/// `namespace` is the element's namespace (`XName.Namespace`): `None` is `XNamespace.None`.
/// An element whose namespace differs from its parent's gets an `xmlns` declaration when
/// written, unless it carries one already as an attribute (as a parsed element does).
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

    /// An element in `namespace`, which may be none (`XNamespace.None + name`).
    pub fn in_namespace(namespace: Option<&str>, name: impl Into<String>) -> Self {
        XElement {
            name: name.into(),
            namespace: namespace.map(str::to_string),
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

    /// `Attribute(name)?.Value`.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(mut self, child: XElement) -> Self {
        self.push(child);
        self
    }

    /// `Add(element)`.
    pub fn push(&mut self, child: XElement) {
        self.add_node(XNode::Element(child));
    }

    pub fn children(mut self, children: impl IntoIterator<Item = XElement>) -> Self {
        for child in children {
            self.push(child);
        }
        self
    }

    /// Text content, as `new XElement(name, "text")`. An empty string still makes an empty
    /// element non-empty: it is written `<name></name>`. Added to an element that already
    /// has content, an empty string is nothing, as in XLinq.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.add_node(XNode::Text(text.into()));
        self
    }

    /// `XContainer.Add` for one node. XLinq keeps an empty string only as the whole content of
    /// an element that had none (`content = ""`), and drops it as soon as a node is added.
    fn add_node(&mut self, node: XNode) {
        if let XNode::Text(t) = &node
            && t.is_empty()
            && !self.content.is_empty()
        {
            return;
        }
        if !matches!(node, XNode::Text(_)) && self.holds_only_empty_text() {
            self.content.clear();
        }
        self.content.push(node);
    }

    fn holds_only_empty_text(&self) -> bool {
        matches!(self.content.as_slice(), [XNode::Text(t)] if t.is_empty())
    }

    /// `AddFirst(elements)`: the elements before everything the element holds.
    pub fn add_first(&mut self, elements: Vec<XElement>) {
        if elements.is_empty() {
            return;
        }
        if self.holds_only_empty_text() {
            self.content.clear();
        }
        let tail = std::mem::take(&mut self.content);
        self.content = elements.into_iter().map(XNode::Element).collect();
        self.content.extend(tail);
    }

    /// `content[index].AddAfterSelf(elements)`: the elements right after the node at `index`.
    pub fn insert_after(&mut self, index: usize, elements: Vec<XElement>) {
        let at = (index + 1).min(self.content.len());
        self.content
            .splice(at..at, elements.into_iter().map(XNode::Element));
    }

    /// Whether this is `{namespace}name`, as `element.Name == ns + name` compares.
    pub fn is(&self, namespace: Option<&str>, name: &str) -> bool {
        self.name == name && self.namespace.as_deref() == namespace
    }

    /// `Elements()`: the child elements, in order.
    pub fn elements(&self) -> impl Iterator<Item = &XElement> {
        self.content.iter().filter_map(|node| match node {
            XNode::Element(e) => Some(e),
            _ => None,
        })
    }

    pub fn elements_mut(&mut self) -> impl Iterator<Item = &mut XElement> {
        self.content.iter_mut().filter_map(|node| match node {
            XNode::Element(e) => Some(e),
            _ => None,
        })
    }

    /// `Elements(ns + name)`.
    pub fn elements_named<'a>(
        &'a self,
        namespace: Option<&'a str>,
        name: &'a str,
    ) -> impl Iterator<Item = &'a XElement> + 'a {
        self.elements().filter(move |e| e.is(namespace, name))
    }

    /// `Descendants(ns + name).FirstOrDefault()`: the first match in document order, not
    /// counting this element itself.
    pub fn first_descendant(&self, namespace: Option<&str>, name: &str) -> Option<&XElement> {
        for child in self.elements() {
            if child.is(namespace, name) {
                return Some(child);
            }
            if let Some(found) = child.first_descendant(namespace, name) {
                return Some(found);
            }
        }
        None
    }

    /// `XElement.Value`: every text node beneath this element, concatenated.
    pub fn value(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        for node in &self.content {
            match node {
                XNode::Text(t) | XNode::CData(t) => out.push_str(t),
                XNode::Element(e) => e.collect_text(out),
                XNode::Comment(_) => {}
            }
        }
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
        let mut declared = false;
        for (k, v) in &self.attributes {
            declared |= k == "xmlns";
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            escape_attribute(out, v);
            out.push('"');
        }
        // XmlWriter declares the element's namespace after its attributes when nothing in
        // scope binds it: `xmlns=".."`, or `xmlns=""` for no namespace under a parent that has one.
        let own = self.namespace.as_deref();
        if !declared && own != parent_ns {
            out.push_str(" xmlns=\"");
            escape_attribute(out, own.unwrap_or(""));
            out.push('"');
        }
        if self.content.is_empty() {
            out.push_str(" />");
            return;
        }
        out.push('>');
        // XmlWriter stops indenting inside an element once it holds text: mixed content is
        // written as it stands.
        let mixed = self
            .content
            .iter()
            .any(|c| matches!(c, XNode::Text(_) | XNode::CData(_)));
        let child_indent = indent && !mixed;
        for node in &self.content {
            match node {
                XNode::Text(t) => escape_text(out, t),
                XNode::CData(t) => {
                    out.push_str("<![CDATA[");
                    out.push_str(t);
                    out.push_str("]]>");
                }
                XNode::Comment(c) => {
                    if child_indent {
                        newline(out, depth + 1);
                    }
                    out.push_str("<!--");
                    out.push_str(c);
                    out.push_str("-->");
                }
                XNode::Element(e) => {
                    if child_indent {
                        newline(out, depth + 1);
                    }
                    e.write(out, depth + 1, own, child_indent);
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

    /// `XDocument.Parse(text).Root`: the document's root element, read as `LoadOptions.None`
    /// reads it.
    ///
    /// - Text made only of whitespace is dropped, and text is kept with its line breaks
    ///   normalised to `\n` and its references resolved.
    /// - An element written `<a></a>` (or holding only whitespace) keeps an empty string as
    ///   its content, so it is written back as `<a></a>`, not `<a />`.
    /// - Attribute values are normalised: a literal tab or line break becomes a space, while
    ///   `&#xA;` stays a line break.
    /// - `xmlns` declarations stay attributes in their place, and every unprefixed element
    ///   takes the default namespace in scope.
    /// - The declaration, processing instructions and a DOCTYPE are not kept (`ToString`
    ///   never writes the declaration; Navidrome writes neither of the others). A prefixed
    ///   name is kept as written, without resolving its prefix.
    ///
    /// Anything that is not well-formed (a second root, text outside the root, an unclosed
    /// or mismatched tag, an unknown entity) is an error, as `XmlException` was.
    pub fn parse(text: &str) -> Result<XElement, XmlParseError> {
        let mut reader = Reader::from_str(text);
        reader.config_mut().check_end_names = true;
        let mut stack: Vec<XElement> = Vec::new();
        let mut root: Option<XElement> = None;
        let mut pending = String::new();

        loop {
            let event = reader
                .read_event()
                .map_err(|e| XmlParseError::new(format!("{e}")))?;
            match event {
                Event::Text(t) => pending.push_str(&t.xml10_content()),
                Event::GeneralRef(r) => pending.push(resolve_reference(&r)?),
                Event::CData(c) => {
                    flush_text(&mut stack, &mut pending)?;
                    let parent = stack
                        .last_mut()
                        .ok_or_else(|| XmlParseError::new("CDATA outside the root element"))?;
                    parent.content.push(XNode::CData(c.xml10_content().into_owned()));
                }
                Event::Comment(c) => {
                    flush_text(&mut stack, &mut pending)?;
                    if let Some(parent) = stack.last_mut() {
                        parent
                            .content
                            .push(XNode::Comment(c.xml10_content().into_owned()));
                    }
                }
                Event::Start(start) => {
                    flush_text(&mut stack, &mut pending)?;
                    let parent_ns = stack.last().and_then(|p| p.namespace.clone());
                    stack.push(read_element(&start, parent_ns)?);
                }
                Event::Empty(start) => {
                    flush_text(&mut stack, &mut pending)?;
                    let parent_ns = stack.last().and_then(|p| p.namespace.clone());
                    let element = read_element(&start, parent_ns)?;
                    close_element(&mut stack, &mut root, element)?;
                }
                Event::End(_) => {
                    flush_text(&mut stack, &mut pending)?;
                    let mut element = stack
                        .pop()
                        .ok_or_else(|| XmlParseError::new("an end tag without a start"))?;
                    // XContainer.ReadContentFrom: an element read from a start and an end tag
                    // with nothing kept between them holds the empty string.
                    if element.content.is_empty() {
                        element.content.push(XNode::Text(String::new()));
                    }
                    close_element(&mut stack, &mut root, element)?;
                }
                Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {
                    flush_text(&mut stack, &mut pending)?;
                }
                Event::Eof => break,
            }
        }
        flush_text(&mut stack, &mut pending)?;
        if !stack.is_empty() {
            return Err(XmlParseError::new("an element is not closed"));
        }
        root.ok_or_else(|| XmlParseError::new("no root element"))
    }
}

/// Why [`XElement::parse`] refused a document (`XmlException`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct XmlParseError(String);

impl XmlParseError {
    fn new(message: impl Into<String>) -> Self {
        XmlParseError(message.into())
    }
}

/// A character or predefined entity reference in text. Anything else is undeclared.
fn resolve_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<char, XmlParseError> {
    match reference.resolve_char_ref() {
        Ok(Some(c)) => Ok(c),
        Ok(None) => match &*reference.xml10_content() {
            "lt" => Ok('<'),
            "gt" => Ok('>'),
            "amp" => Ok('&'),
            "apos" => Ok('\''),
            "quot" => Ok('"'),
            other => Err(XmlParseError::new(format!("undeclared entity &{other};"))),
        },
        Err(e) => Err(XmlParseError::new(format!("{e}"))),
    }
}

/// The text read since the last markup, as a text node of the open element. Whitespace alone
/// is dropped (`XmlReaderSettings.IgnoreWhitespace`); anything else outside the root is an error.
fn flush_text(stack: &mut [XElement], pending: &mut String) -> Result<(), XmlParseError> {
    if pending.is_empty() {
        return Ok(());
    }
    let text = std::mem::take(pending);
    if text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
        return Ok(());
    }
    match stack.last_mut() {
        Some(parent) => {
            parent.content.push(XNode::Text(text));
            Ok(())
        }
        None => Err(XmlParseError::new("text outside the root element")),
    }
}

fn read_element(start: &BytesStart<'_>, parent_ns: Option<String>) -> Result<XElement, XmlParseError> {
    let mut element = XElement::new(start.name().0.to_string());
    element.namespace = parent_ns;
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|e| XmlParseError::new(format!("{e}")))?;
        let key = attribute.key.0.to_string();
        let value = match attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| XmlParseError::new(format!("{e}")))?
        {
            Cow::Borrowed(v) => v.to_string(),
            Cow::Owned(v) => v,
        };
        if key == "xmlns" {
            element.namespace = (!value.is_empty()).then(|| value.clone());
        }
        element.attributes.push((key, value));
    }
    Ok(element)
}

fn close_element(
    stack: &mut [XElement],
    root: &mut Option<XElement>,
    element: XElement,
) -> Result<(), XmlParseError> {
    match stack.last_mut() {
        Some(parent) => {
            parent.content.push(XNode::Element(element));
            Ok(())
        }
        None if root.is_none() => {
            *root = Some(element);
            Ok(())
        }
        None => Err(XmlParseError::new("a second root element")),
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

    /// Each input, and what `XDocument.Parse(input).ToString()` gave on .NET 9.
    #[test]
    fn parse_then_write_matches_xdocument_parse_to_string() {
        let cases: &[(&str, &str)] = &[
            (
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\"><searchResult3><artist id=\"a\" name=\"A &amp; B\"><roles>artist</roles></artist><album id=\"x\"><genres name=\"Rock\"></genres><b/></album></searchResult3></subsonic-response>",
                concat!(
                    "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\">\n",
                    "  <searchResult3>\n",
                    "    <artist id=\"a\" name=\"A &amp; B\">\n",
                    "      <roles>artist</roles>\n",
                    "    </artist>\n",
                    "    <album id=\"x\">\n",
                    "      <genres name=\"Rock\"></genres>\n",
                    "      <b />\n",
                    "    </album>\n",
                    "  </searchResult3>\n",
                    "</subsonic-response>"
                ),
            ),
            (
                "<r a=\"x\ty\nz&#xA;w&#9;\" b='q&quot;'>  <c>  t  </c> <d>   </d><e>x &lt; y &#x1F600; &amp;</e>\r\n<f>a\r\nb\rc</f><g><!-- note --></g><h><![CDATA[ <x> ]]></h></r>",
                concat!(
                    "<r a=\"x y z&#xA;w&#x9;\" b=\"q&quot;\">\n",
                    "  <c>  t  </c>\n",
                    "  <d></d>\n",
                    "  <e>x &lt; y \u{1F600} &amp;</e>\n",
                    "  <f>a\nb\nc</f>\n",
                    "  <g>\n",
                    "    <!-- note -->\n",
                    "  </g>\n",
                    "  <h><![CDATA[ <x> ]]></h>\n",
                    "</r>"
                ),
            ),
            (
                "<r xmlns=\"urn:a\"><s xmlns=\"\"><t/></s><u xmlns=\"urn:b\" k=\"v\"/></r>",
                "<r xmlns=\"urn:a\">\n  <s xmlns=\"\">\n    <t />\n  </s>\n  <u xmlns=\"urn:b\" k=\"v\" />\n</r>",
            ),
            ("<r><a>x<b/>y</a></r>", "<r>\n  <a>x<b />y</a>\n</r>"),
        ];
        for (input, want) in cases {
            let parsed = XElement::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(parsed.to_xml_string(), *want, "for {input}");
        }
    }

    #[test]
    fn adding_to_an_element_read_empty_drops_its_empty_string() {
        // .NET 9: AddFirst into <c></c>, and an element in no namespace added under one.
        let mut root = XElement::parse("<r xmlns=\"urn:a\"><c></c></r>").expect("parses");
        let c = root.elements_mut().next().expect("c");
        c.add_first(vec![XElement::ns("urn:a", "n")]);
        root.push(XElement::new("plain").child(XElement::new("inner")));
        assert_eq!(
            root.to_xml_string(),
            "<r xmlns=\"urn:a\">\n  <c>\n    <n />\n  </c>\n  <plain xmlns=\"\">\n    <inner />\n  </plain>\n</r>"
        );
    }

    #[test]
    fn parse_reads_namespaces_values_and_descendants() {
        let root = XElement::parse(
            "<subsonic-response xmlns=\"http://subsonic.org/restapi\"><a><searchResult3><song id=\"1\"/></searchResult3></a></subsonic-response>",
        )
        .expect("parses");
        assert_eq!(root.namespace.as_deref(), Some(NS));
        let found = root.first_descendant(Some(NS), "searchResult3").expect("found");
        assert_eq!(found.elements_named(Some(NS), "song").count(), 1);
        assert!(root.first_descendant(None, "searchResult3").is_none());
        let text = XElement::parse("<a>x<b>y</b><![CDATA[z]]><!--c--></a>").expect("parses");
        assert_eq!(text.value(), "xyz");
        assert_eq!(text.attribute("missing"), None);
    }

    #[test]
    fn parse_refuses_what_is_not_well_formed() {
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "x<a/>",
            "<a>&nbsp;</a>",
            "<a b=\"1\" b=\"2\"/>",
            "{\"json\":true}",
        ] {
            assert!(XElement::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
