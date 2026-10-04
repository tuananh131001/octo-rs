//! What a builder method answers: the bytes and content type of the ASP.NET result it
//! returned, ready to go out as an axum response.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::xml::XElement;

/// Which ASP.NET result the C# built, which decides the content type it went out with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// `JsonResult`: `application/json; charset=utf-8`, the default encoder's escaping.
    Json,
    /// `ContentResult`: the content type exactly as given, no charset added
    /// (`application/xml` for every Octo-built XML document).
    Content,
    /// `FileContentResult`: bytes passed through as they came.
    File,
}

/// One Subsonic answer. Always HTTP 200: Subsonic errors travel inside the envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsonicReply {
    pub kind: ReplyKind,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The content type `JsonResult` wrote.
pub const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

impl SubsonicReply {
    /// `new JsonResult(value)`: compact, with the default encoder's escaping (`isn\u0027t`).
    pub fn json(value: &Value) -> Self {
        Self::json_text(octo_core::json::to_string(value))
    }

    /// A `JsonResult` whose JSON is already written (a tree with numbers kept as written).
    pub fn json_text(text: String) -> Self {
        SubsonicReply {
            kind: ReplyKind::Json,
            content_type: JSON_CONTENT_TYPE.to_string(),
            body: text.into_bytes(),
        }
    }

    /// `new ContentResult { Content = doc.ToString(), ContentType = "application/xml" }`.
    pub fn xml(document: &XElement) -> Self {
        Self::content(document.to_xml_string(), "application/xml")
    }

    /// `new ContentResult { Content = content, ContentType = contentType }`.
    pub fn content(content: String, content_type: &str) -> Self {
        SubsonicReply {
            kind: ReplyKind::Content,
            content_type: content_type.to_string(),
            body: content.into_bytes(),
        }
    }

    /// `new FileContentResult(bytes, contentType)`.
    pub fn file(bytes: Vec<u8>, content_type: &str) -> Self {
        SubsonicReply {
            kind: ReplyKind::File,
            content_type: content_type.to_string(),
            body: bytes,
        }
    }

    /// The body as text (lossy for a passed-through body that is not UTF-8).
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// The body read back as JSON, for tests and callers that inspect an answer.
    pub fn json_value(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

impl IntoResponse for SubsonicReply {
    /// 200 with the content type as the result wrote it and an explicit `Content-Length`.
    /// (`JsonResult` went out chunked; length and chunked framing are the same to a client,
    /// see known-diffs.md.)
    fn into_response(self) -> Response {
        let length = self.body.len();
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        // A content type passed through from upstream that is not a valid header value
        // falls back to the octet-stream ASP.NET would have refused it for.
        let content_type = HeaderValue::from_str(&self.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
        headers.insert(header::CONTENT_TYPE, content_type);
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_answers_200_with_its_content_type_and_length() {
        let cases = [
            (
                SubsonicReply::json(&serde_json::json!({"a": "isn't"})),
                JSON_CONTENT_TYPE,
                18,
            ),
            (SubsonicReply::xml(&XElement::new("x")), "application/xml", 5),
            (
                SubsonicReply::file(vec![1, 2, 3], "application/json"),
                "application/json",
                3,
            ),
        ];
        for (reply, content_type, length) in cases {
            let response = reply.into_response();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                length.to_string().as_str()
            );
        }
        assert_eq!(
            SubsonicReply::json(&serde_json::json!({"a": "isn't"})).text(),
            r#"{"a":"isn\u0027t"}"#
        );
    }
}
