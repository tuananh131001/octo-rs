//! Plumbing the AdminController actions share: the browse sign-in (`BrowseUser` and its
//! cookie), the secret placeholder, `[FromBody]` binding as `[ApiController]` did it, and JSON
//! answers written the way MVC wrote them.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use octo_core::json::dom::Node;
use octo_core::settings::JsonObject;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::AppState;
use crate::http::error::{problem, validation_problem};
use crate::middleware::forwarded::RequestScheme;
use crate::services::admin::browse_session_store::BrowseSessionStore;

/// What a saved Navidrome admin password reads as through the admin API. The Last.fm shared
/// secret and each listener's Last.fm session key read the same way: they were added after
/// this was, and neither has ever gone out in clear. Every other secret still does, as it
/// always has.
pub const SECRET_PLACEHOLDER: &str = "(saved, not shown)";

/// Cookie carrying the browse session. Scoped to /api/admin so it is never sent with the
/// Subsonic traffic Octo proxies.
pub const BROWSE_COOKIE_NAME: &str = "octo_browse";

/// The header a script signs in with instead of the cookie.
pub const BROWSE_TOKEN_HEADER: &str = "X-Octo-Browse-Token";

/// `MaskSecret`: the placeholder for a saved secret, `""` for none.
pub fn mask_secret(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => SECRET_PLACEHOLDER.to_string(),
        _ => String::new(),
    }
}

// ---- the browse sign-in ----------------------------------------------------------------

/// `Request.Cookies[name]`: the first cookie of that name, unescaped as ASP.NET unescaped
/// cookie values.
pub fn request_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split([';', ',']) {
            let Some((key, raw)) = pair.split_once('=') else {
                continue;
            };
            if key.trim() != name {
                continue;
            }
            let raw = raw.trim();
            let raw = raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(raw);
            return Some(octo_core::common::dotnet::unescape_data_string(raw));
        }
    }
    None
}

/// A request header's value as `[FromHeader]` bound a string: the first one, or None.
pub fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// A `[FromQuery] string?` parameter: the first value of the name, matched ignoring case, and
/// an empty value read as missing (`ConvertEmptyStringToNull`).
pub fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// `Request.IsHttps`, after `UseForwardedHeaders` applied `X-Forwarded-Proto`.
pub fn is_https(scheme: Option<&RequestScheme>) -> bool {
    scheme.is_some_and(|s| s.0.eq_ignore_ascii_case("https"))
}

/// The cookie's terms (`BrowseCookieOptions`). Secure only over HTTPS, since this is normally
/// reached over plain HTTP on a LAN and a Secure cookie would simply be dropped there. Its age
/// is set again on every signed-in request, so the browser keeps it exactly as long as the
/// server keeps the session. Written in `SetCookieHeaderValue`'s order.
pub fn browse_cookie(token: &str, https: bool) -> HeaderValue {
    let max_age = BrowseSessionStore::TTL.num_seconds();
    let secure = if https { "; secure" } else { "" };
    let text = format!(
        "{BROWSE_COOKIE_NAME}={}; max-age={max_age}; path=/api/admin{secure}; samesite=strict; httponly",
        escape_cookie_value(token)
    );
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("octo_browse="))
}

/// `Response.Cookies.Delete(name, new CookieOptions { Path = "/api/admin" })`.
pub fn browse_cookie_deleted() -> HeaderValue {
    HeaderValue::from_static("octo_browse=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/api/admin")
}

/// `Uri.EscapeDataString`, which ASP.NET applied to a cookie value it wrote.
fn escape_cookie_value(value: &str) -> String {
    octo_core::common::dotnet::escape_data_string(value)
}

/// The Navidrome admin this request is signed in as (`BrowseUser`), with the cookie to send
/// back. Cookie first (how the admin UI signs in), header second so the endpoints stay usable
/// from curl. A live cookie is renewed: [`BrowseUser::finish`] adds it to the answer.
pub struct BrowseUser {
    pub user: Option<String>,
    renew: Option<HeaderValue>,
}

impl BrowseUser {
    pub fn check(
        state: &AppState,
        headers: &HeaderMap,
        scheme: Option<&RequestScheme>,
        header_token: Option<&str>,
    ) -> BrowseUser {
        let cookie = request_cookie(headers, BROWSE_COOKIE_NAME);
        // A stale cookie hides a valid header: the cookie wins whenever there is one.
        let user = state.browse_sessions.user_of(cookie.as_deref().or(header_token));
        let renew = match (&user, &cookie) {
            (Some(_), Some(cookie)) => Some(browse_cookie(cookie, is_https(scheme))),
            _ => None,
        };
        BrowseUser { user, renew }
    }

    /// `HasBrowseSession`.
    pub fn signed_in(&self) -> bool {
        self.user.is_some()
    }

    /// The answer with the renewed cookie, when there is one.
    pub fn finish(&self, mut res: Response) -> Response {
        if let Some(cookie) = &self.renew {
            res.headers_mut().append(header::SET_COOKIE, cookie.clone());
        }
        res
    }
}

// ---- JSON answers ----------------------------------------------------------------------

/// A JSON document already written as text, with the content type an MVC result gave it.
pub fn json_text(status: StatusCode, text: String, content_type: &'static str) -> Response {
    let mut res = Response::new(Body::from(text));
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

/// `new JsonResult(node)` / `Ok(node)` over a `JsonNode` tree: compact, default escaping,
/// numbers as written.
pub fn json_node(status: StatusCode, node: &Node) -> Response {
    json_text(
        status,
        node.to_json_string(false),
        "application/json; charset=utf-8",
    )
}

/// An MVC `ObjectResult` (`Ok(obj)`, `BadRequest(obj)`, `Accepted(obj)`, `Conflict(obj)`,
/// `StatusCode(n, obj)`, ...). In .NET 9 its output formatter swaps the default encoder for
/// `UnsafeRelaxedJsonEscaping` when none is configured, so `'`, `+` and non-ASCII letters go
/// out as they are, while `new JsonResult(obj)` (written with [`crate::http::error::json_ok`])
/// escapes them. The C# baseline shows both: `browse` and `lastfm/scrobble/disconnect` are not
/// escaped, `settings`, `status` and `config-sources` are.
pub fn object_result(status: StatusCode, body: &impl serde::Serialize) -> Response {
    crate::http::error::json_relaxed_response(status, body, "application/json; charset=utf-8")
}

/// `Ok(obj)`: [`object_result`] with 200.
pub fn ok(body: &impl serde::Serialize) -> Response {
    object_result(StatusCode::OK, body)
}

/// `{"error": message}` with a status, as `BadRequest(new { error })` and its kin wrote it.
pub fn error_json(status: StatusCode, message: &str) -> Response {
    object_result(status, &serde_json::json!({ "error": message }))
}

/// STJ's camelCase naming policy (`JsonNamingPolicy.CamelCase`) for one property name.
pub fn camel_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.is_empty() || !chars[0].is_uppercase() {
        return name.to_string();
    }
    let mut out = chars.clone();
    for i in 0..chars.len() {
        if i == 1 && !chars[i].is_uppercase() {
            break;
        }
        let has_next = i + 1 < chars.len();
        // Stop when the next character is not upper case: the last capital of a run starts
        // the next word ("XMLValue" → "xmlValue").
        if i > 0 && has_next && !chars[i + 1].is_uppercase() {
            if chars[i + 1] == ' ' {
                out[i] = octo_core::common::dotnet::to_lower_char(chars[i]);
            }
            break;
        }
        out[i] = octo_core::common::dotnet::to_lower_char(chars[i]);
    }
    out.into_iter().collect()
}

/// A value serialised with its state-file (PascalCase) names, renamed as an API answer writes
/// it: every property name camelCased, except the keys of the dictionaries named in
/// `dictionaries` (by their camelCased property name), which STJ left alone.
pub fn camel_case_value(value: Value, dictionaries: &[&str]) -> Value {
    fn walk(value: Value, dictionaries: &[&str], is_dictionary: bool) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        if is_dictionary {
                            (k, walk(v, dictionaries, false))
                        } else {
                            let name = camel_case(&k);
                            let child_is_dictionary = dictionaries.contains(&name.as_str());
                            (name, walk(v, dictionaries, child_is_dictionary))
                        }
                    })
                    .collect(),
            ),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| walk(v, dictionaries, false)).collect())
            }
            other => other,
        }
    }
    walk(value, dictionaries, false)
}

// ---- JsonNode helpers -------------------------------------------------------------------

pub fn n_str(value: &str) -> Node {
    Node::String(value.to_string())
}

pub fn n_num(value: impl std::fmt::Display) -> Node {
    Node::Number(value.to_string())
}

pub fn n_obj<'a>(pairs: impl IntoIterator<Item = (&'a str, Node)>) -> Node {
    Node::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn n_str_array<S: AsRef<str>>(items: &[S]) -> Node {
    Node::Array(items.iter().map(|s| n_str(s.as_ref())).collect())
}

/// `KeyOf`: the key in `obj` that matches `name` ignoring case, as configuration keys do.
pub fn key_of(obj: &JsonObject, name: &str) -> Option<String> {
    obj.keys()
        .find(|key| octo_core::common::dotnet::eq_ignore_case(key, name))
        .cloned()
}

/// `Child`: the object under `name` (ignoring case), or None.
pub fn child<'a>(obj: &'a JsonObject, name: &str) -> Option<&'a JsonObject> {
    key_of(obj, name)
        .and_then(|key| obj.get(&key))
        .and_then(Node::as_object)
}

pub fn child_mut<'a>(obj: &'a mut JsonObject, name: &str) -> Option<&'a mut JsonObject> {
    let key = key_of(obj, name)?;
    obj.get_mut(&key).and_then(Node::as_object_mut)
}

// ---- [FromBody] --------------------------------------------------------------------------

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let media = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    media == "application/json"
        || media == "text/json"
        || (media.starts_with("application/") && media.ends_with("+json"))
}

/// A `[FromBody] T name` parameter on an `[ApiController]` action: the JSON input formatter
/// (case-insensitive names, numbers from strings), with the automatic answers it gave before
/// the action ran. A body that is not JSON by its content type is a 415; an empty one, `null`
/// or one that does not parse is the 400 `ValidationProblemDetails`.
#[allow(clippy::result_large_err)]
pub fn bind_body<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8], name: &str) -> Result<T, Response> {
    if !is_json_content_type(headers) {
        return Err(problem(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }
    let required = format!("The {name} field is required.");
    if body.is_empty() {
        return Err(validation_problem(&[
            ("", &["A non-empty request body is required."]),
            (name, &[required.as_str()]),
        ]));
    }
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if let Some(error) = stj_parse_error(text) {
        let message = format!("{} Path: $ | {}", error.message, error.position());
        return Err(validation_problem(&[
            ("$", &[message.as_str()]),
            (name, &[required.as_str()]),
        ]));
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            let message = e.to_string();
            return Err(validation_problem(&[
                ("$", &[message.as_str()]),
                (name, &[required.as_str()]),
            ]));
        }
    };
    if value.is_null() {
        return Err(validation_problem(&[(name, &[required.as_str()])]));
    }
    octo_core::json::web::from_value(&value).map_err(|e| {
        let message = format!("The JSON value could not be converted. {e}");
        validation_problem(&[("$", &[message.as_str()]), (name, &[required.as_str()])])
    })
}

/// The body of an action that read `Request.Body` itself (`new StreamReader(Request.Body)`):
/// UTF-8 with a BOM dropped.
pub fn body_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
}

/// `JsonNode.Parse(text)` with the reader's default options (no comments, no trailing commas):
/// the tree, or the exception's message.
pub fn parse_json_node(text: &str) -> Result<Node, String> {
    if let Some(error) = stj_parse_error(text) {
        return Err(format!("{} {}", error.message, error.position()));
    }
    Node::parse(text)
}

// ---- Utf8JsonReader's error messages ----------------------------------------------------------

/// Where and why `Utf8JsonReader` refused a payload, in the words of its `JsonReaderException`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StjReadError {
    pub message: String,
    pub line: usize,
    pub byte_in_line: usize,
}

impl StjReadError {
    pub fn position(&self) -> String {
        format!(
            "LineNumber: {} | BytePositionInLine: {}.",
            self.line, self.byte_in_line
        )
    }
}

/// Reads `text` as `Utf8JsonReader` does with its default options (one value, no comments,
/// no trailing commas, depth at most 64) and returns its first complaint, or None when it is
/// valid JSON. The common refusals carry System.Text.Json's own messages; the rarer ones a
/// close paraphrase.
pub fn stj_parse_error(text: &str) -> Option<StjReadError> {
    let mut reader = StjReader {
        s: text.as_bytes(),
        i: 0,
        line: 0,
        line_start: 0,
    };
    reader.document().err()
}

struct StjReader<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
    line_start: usize,
}

const MAX_DEPTH: usize = 64;

fn printable(b: u8) -> String {
    if (0x20..0x7f).contains(&b) {
        (b as char).to_string()
    } else {
        format!("0x{b:02X}")
    }
}

impl StjReader<'_> {
    fn fail(&self, message: impl Into<String>) -> StjReadError {
        StjReadError {
            message: message.into(),
            line: self.line,
            byte_in_line: self.i - self.line_start,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.s.get(self.i) {
            match b {
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'\n' => {
                    self.i += 1;
                    self.line += 1;
                    self.line_start = self.i;
                }
                _ => break,
            }
        }
    }

    fn document(&mut self) -> Result<(), StjReadError> {
        self.skip_ws();
        if self.i >= self.s.len() {
            return Err(self.fail(
                "The input does not contain any JSON tokens. Expected the input to start with a valid JSON token, while isReadFinalBlock is true.",
            ));
        }
        self.value(0)?;
        self.skip_ws();
        if let Some(&b) = self.s.get(self.i) {
            return Err(self.fail(format!(
                "'{}' is invalid after a single JSON value. Expected end of data.",
                printable(b)
            )));
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<(), StjReadError> {
        let Some(&b) = self.s.get(self.i) else {
            return Err(self.fail("Expected a value, but instead reached end of data."));
        };
        match b {
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'"' => self.string(),
            b'-' | b'0'..=b'9' => self.number(),
            b't' => self.literal("true"),
            b'f' => self.literal("false"),
            b'n' => self.literal("null"),
            _ => Err(self.fail(format!("'{}' is an invalid start of a value.", printable(b)))),
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), StjReadError> {
        if depth > MAX_DEPTH {
            return Err(self.fail(format!(
                "The maximum configured depth of {MAX_DEPTH} has been exceeded. Cannot read next JSON object."
            )));
        }
        Ok(())
    }

    fn object(&mut self, depth: usize) -> Result<(), StjReadError> {
        self.check_depth(depth)?;
        self.i += 1;
        self.skip_ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            match self.s.get(self.i) {
                None => {
                    return Err(self.fail(
                        "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.",
                    ));
                }
                Some(b'"') => self.string()?,
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is an invalid start of a property name. Expected a '\"'.",
                        printable(b)
                    )));
                }
            }
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b':') => self.i += 1,
                None => {
                    return Err(self.fail(
                        "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.",
                    ));
                }
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid after a property name. Expected a ':'.",
                        printable(b)
                    )));
                }
            }
            self.skip_ws();
            self.value(depth)?;
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b',') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        return Err(self.fail(
                            "The JSON object contains a trailing comma at the end which is not supported in this mode. Change the reader options.",
                        ));
                    }
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                None => {
                    return Err(self.fail(
                        "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.",
                    ));
                }
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid after a value. Expected either ',', '}}', or ']'.",
                        printable(b)
                    )));
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), StjReadError> {
        self.check_depth(depth)?;
        self.i += 1;
        self.skip_ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(());
        }
        loop {
            if self.i >= self.s.len() {
                return Err(self.fail(
                    "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.",
                ));
            }
            self.value(depth)?;
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b',') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.s.get(self.i) == Some(&b']') {
                        return Err(self.fail(
                            "The JSON array contains a trailing comma at the end which is not supported in this mode. Change the reader options.",
                        ));
                    }
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                None => {
                    return Err(self.fail(
                        "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.",
                    ));
                }
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid after a value. Expected either ',', '}}', or ']'.",
                        printable(b)
                    )));
                }
            }
        }
    }

    fn string(&mut self) -> Result<(), StjReadError> {
        self.i += 1;
        loop {
            let Some(&b) = self.s.get(self.i) else {
                return Err(self.fail("Expected end of string, but instead reached end of data."));
            };
            match b {
                b'"' => {
                    self.i += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.i += 1;
                    match self.s.get(self.i) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                        Some(b'u') => {
                            self.i += 1;
                            for _ in 0..4 {
                                match self.s.get(self.i) {
                                    Some(h) if h.is_ascii_hexdigit() => self.i += 1,
                                    Some(&h) => {
                                        return Err(self.fail(format!(
                                            "'{}' is not a hex digit following '\\u' within a JSON string. The string should be correctly escaped.",
                                            printable(h)
                                        )));
                                    }
                                    None => {
                                        return Err(self.fail(
                                            "Expected end of string, but instead reached end of data.",
                                        ));
                                    }
                                }
                            }
                        }
                        Some(&e) => {
                            return Err(self.fail(format!(
                                "'{}' is an invalid escapable character within a JSON string. The string should be correctly escaped.",
                                printable(e)
                            )));
                        }
                        None => {
                            return Err(self.fail("Expected end of string, but instead reached end of data."));
                        }
                    }
                }
                0x00..=0x1f => {
                    return Err(self.fail(format!(
                        "'{}' is invalid within a JSON string. The string should be correctly escaped.",
                        printable(b)
                    )));
                }
                _ => self.i += 1,
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<(), StjReadError> {
        if self.s[self.i] == b'-' {
            self.i += 1;
            match self.s.get(self.i) {
                Some(b) if b.is_ascii_digit() => {}
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid within a number, immediately after a sign character ('+' or '-'). Expected a digit ('0'-'9').",
                        printable(b)
                    )));
                }
                None => return Err(self.fail("Expected a digit ('0'-'9'), but instead reached end of data.")),
            }
        }
        if self.s[self.i] == b'0' {
            self.i += 1;
            if let Some(&b) = self.s.get(self.i)
                && b.is_ascii_digit()
            {
                return Err(self.fail(format!("Invalid leading zero before '{}'.", printable(b))));
            }
        } else {
            self.digits();
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            match self.s.get(self.i) {
                Some(b) if b.is_ascii_digit() => {
                    self.digits();
                }
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid within a number, immediately after a decimal point ('.'). Expected a digit ('0'-'9').",
                        printable(b)
                    )));
                }
                None => return Err(self.fail("Expected a digit ('0'-'9'), but instead reached end of data.")),
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            match self.s.get(self.i) {
                Some(b) if b.is_ascii_digit() => {
                    self.digits();
                }
                Some(&b) => {
                    return Err(self.fail(format!(
                        "'{}' is invalid within a number, immediately after a sign character ('+' or '-'). Expected a digit ('0'-'9').",
                        printable(b)
                    )));
                }
                None => return Err(self.fail("Expected a digit ('0'-'9'), but instead reached end of data.")),
            }
        }
        match self.s.get(self.i) {
            None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']') => Ok(()),
            Some(&b) => Err(self.fail(format!(
                "'{}' is an invalid end of a number. Expected a delimiter.",
                printable(b)
            ))),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), StjReadError> {
        let bytes = word.as_bytes();
        let available = &self.s[self.i..];
        if available.starts_with(bytes) {
            self.i += bytes.len();
            return Ok(());
        }
        // The reader names what it found, up to the literal's length.
        let found: String = available
            .iter()
            .take(bytes.len())
            .map(|b| printable(*b))
            .collect();
        Err(self.fail(format!(
            "'{found}' is an invalid JSON literal. Expected the literal '{word}'."
        )))
    }
}

/// `Accepted(new { ... })`.
pub fn accepted(body: &impl serde::Serialize) -> Response {
    object_result(StatusCode::ACCEPTED, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stj_messages_name_the_first_offending_byte() {
        let cases = [
            (
                "{not json",
                "'n' is an invalid start of a property name. Expected a '\"'. LineNumber: 0 | BytePositionInLine: 1.",
            ),
            (
                "[1,]",
                "The JSON array contains a trailing comma at the end which is not supported in this mode. Change the reader options. LineNumber: 0 | BytePositionInLine: 3.",
            ),
            (
                "{\"a\":1}x",
                "'x' is invalid after a single JSON value. Expected end of data. LineNumber: 0 | BytePositionInLine: 7.",
            ),
            (
                "{\n  \"a\": x}",
                "'x' is an invalid start of a value. LineNumber: 1 | BytePositionInLine: 7.",
            ),
            (
                "{\"a\":1",
                "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed. LineNumber: 0 | BytePositionInLine: 6.",
            ),
        ];
        for (input, expected) in cases {
            let error = stj_parse_error(input).unwrap_or_else(|| panic!("{input} is refused"));
            assert_eq!(
                format!("{} {}", error.message, error.position()),
                expected,
                "{input}"
            );
        }
        for valid in [
            "{}",
            "[]",
            "null",
            " {\"a\": [1, -2.5e3, true, false, null, \"\\u00e9\"]} ",
        ] {
            assert_eq!(stj_parse_error(valid), None, "{valid}");
        }
    }

    #[test]
    fn camel_case_follows_the_stj_policy() {
        for (input, expected) in [
            ("Url", "url"),
            ("URLValue", "urlValue"),
            ("ID", "id"),
            ("already", "already"),
            ("StageSeconds", "stageSeconds"),
            ("", ""),
        ] {
            assert_eq!(camel_case(input), expected, "{input}");
        }
    }

    #[test]
    fn camel_case_value_leaves_dictionary_keys_alone() {
        let value = serde_json::json!({"Fields": {"AlbumArtist": {"Value": "x"}}, "Notes": ["A"]});
        let renamed = camel_case_value(value, &["fields"]);
        assert_eq!(
            renamed,
            serde_json::json!({"fields": {"AlbumArtist": {"value": "x"}}, "notes": ["A"]})
        );
    }

    #[test]
    fn cookies_are_read_first_wins_and_unescaped() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; octo_browse=AB%2BC; octo_browse=x"),
        );
        assert_eq!(request_cookie(&headers, "octo_browse").as_deref(), Some("AB+C"));
        assert_eq!(request_cookie(&headers, "missing"), None);
    }

    #[test]
    fn the_cookie_is_written_in_set_cookie_header_value_order() {
        assert_eq!(
            browse_cookie("ABC", false).to_str().unwrap(),
            "octo_browse=ABC; max-age=7776000; path=/api/admin; samesite=strict; httponly"
        );
        assert_eq!(
            browse_cookie("ABC", true).to_str().unwrap(),
            "octo_browse=ABC; max-age=7776000; path=/api/admin; secure; samesite=strict; httponly"
        );
    }
}
