//! What the admin controllers of task 6-B2 share: the browse-session checks (`AdminController`'s
//! `BrowseUser`, which re-issues the cookie, and the `Validate` the cover-upgrade and lyrics
//! controllers call, which does not), and the parts of ASP.NET's model binding the actions relied
//! on (`[FromBody]` with `[ApiController]`'s automatic 400s, `[FromQuery]`, `[FromHeader]`).

use axum::http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use chrono::{DateTime, Utc};
use octo_core::common::dotnet::{eq_ignore_case, unescape_data_string};
use octo_core::json::datetime::format_utc;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::AppState;
use crate::http::error::{AppError, problem, validation_problem};
use crate::middleware::forwarded::RequestScheme;
use crate::services::admin::BrowseSessionStore;

/// `AdminController.BrowseCookieName`: the cookie carrying the browse session. Scoped to
/// /api/admin so it is never sent with the Subsonic traffic Octo proxies.
pub const BROWSE_COOKIE_NAME: &str = "octo_browse";

/// The header the endpoints also accept a session token from, so they stay usable from curl.
pub const BROWSE_TOKEN_HEADER: &str = "X-Octo-Browse-Token";

/// `SignInFirst`: what every session-gated endpoint answers without a session.
pub const SIGN_IN_FIRST: &str = "Sign in with your Navidrome admin account first.";

/// `Unauthorized(new { error = SignInFirst })`.
pub fn sign_in() -> Response {
    error(StatusCode::UNAUTHORIZED, SIGN_IN_FIRST)
}

/// `Ok(new { ... })`: an `ObjectResult`, which `SystemTextJsonOutputFormatter` writes with
/// `UnsafeRelaxedJsonEscaping` when no encoder is configured (non-ASCII, `&`, `<`, `'` go out as
/// they are; the parity baseline shows it). A `JsonResult` used the default encoder instead.
pub fn ok(body: &impl serde::Serialize) -> Response {
    status(StatusCode::OK, body)
}

/// `StatusCode(n, new { ... })`, `Accepted(..)`, `BadRequest(..)` and the rest: an
/// `ObjectResult` with a status.
pub fn status(code: StatusCode, body: &impl serde::Serialize) -> Response {
    crate::http::error::json_relaxed_response(code, body, "application/json; charset=utf-8")
}

/// `StatusCode(n, new { error })` and its shorthands (`BadRequest`, `Conflict`, ...).
pub fn error(code: StatusCode, message: impl Into<String>) -> Response {
    status(code, &serde_json::json!({ "error": message.into() }))
}

/// `Request.Cookies[name]`: the first cookie of that name, its value URL-decoded.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then(|| {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                unescape_data_string(value)
            })
        })
        .next()
}

/// `[FromHeader(Name = ...)] string?`: the header's first value; an empty one binds as null.
pub fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `[FromQuery] string?`: the first value of the key, matched ignoring case; an empty value
/// binds as null (`ConvertEmptyStringToNull`).
pub fn query(uri: &Uri, name: &str) -> Option<String> {
    query_raw(uri, name).filter(|v| !v.is_empty())
}

/// The first value of a query key, matched ignoring case, empty kept.
pub fn query_raw(uri: &Uri, name: &str) -> Option<String> {
    let q = uri.query()?;
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| eq_ignore_case(k, name))
        .map(|(_, v)| v.into_owned())
}

/// `[FromQuery] bool name` (default false): `bool.Parse` on the first value. An unparsable one
/// is a model-binding error, which `[ApiController]` answered with a 400 before the action ran.
pub fn query_bool(uri: &Uri, name: &str) -> Result<bool, Rejection> {
    let Some(value) = query_raw(uri, name) else {
        return Ok(false);
    };
    let trimmed = value.trim();
    if eq_ignore_case(trimmed, "true") {
        Ok(true)
    } else if eq_ignore_case(trimmed, "false") {
        Ok(false)
    } else if trimmed.is_empty() {
        let message = format!("The value '{value}' is invalid.");
        Err(Box::new(validation_problem(&[(name, &[message.as_str()])])))
    } else {
        let message = format!("The value '{value}' is not valid.");
        Err(Box::new(validation_problem(&[(name, &[message.as_str()])])))
    }
}

/// `Request.IsHttps`, after `UseForwardedHeaders` took the scheme from `X-Forwarded-Proto`.
pub fn is_https(parts: &Parts) -> bool {
    parts
        .extensions
        .get::<RequestScheme>()
        .is_some_and(|s| eq_ignore_case(&s.0, "https"))
}

/// `Response.Cookies.Append(BrowseCookieName, token, BrowseCookieOptions())`: HttpOnly,
/// SameSite=Strict, Secure only over HTTPS, Path=/api/admin, Max-Age the session's 90 days, in
/// the attribute order ASP.NET writes them.
pub fn browse_cookie(token: &str, secure: bool) -> HeaderValue {
    let max_age = BrowseSessionStore::TTL.num_seconds();
    let secure = if secure { "; secure" } else { "" };
    let value = format!(
        "{BROWSE_COOKIE_NAME}={}; max-age={max_age}; path=/api/admin{secure}; samesite=strict; httponly",
        octo_core::common::dotnet::escape_data_string(token)
    );
    HeaderValue::from_str(&value).expect("an escaped cookie is a valid header value")
}

/// The outcome of `AdminController.BrowseUser`: who is signed in, and the renewed cookie the
/// response carries whatever it says.
pub struct BrowseUser {
    pub user: Option<String>,
    renewed: Option<HeaderValue>,
}

impl BrowseUser {
    /// `BrowseUser(headerToken)`: cookie first (how the admin UI signs in), header second so
    /// the endpoints stay usable from curl. A live cookie is renewed.
    pub fn check(state: &AppState, parts: &Parts) -> BrowseUser {
        let cookie = cookie(&parts.headers, BROWSE_COOKIE_NAME);
        let token = cookie
            .clone()
            .or_else(|| header(&parts.headers, BROWSE_TOKEN_HEADER));
        let user = state.browse_sessions.user_of(token.as_deref());
        let renewed = match (&user, &cookie) {
            (Some(_), Some(cookie)) => Some(browse_cookie(cookie, is_https(parts))),
            _ => None,
        };
        BrowseUser { user, renewed }
    }

    /// `HasBrowseSession(headerToken)`.
    pub fn signed_in(&self) -> bool {
        self.user.is_some()
    }

    /// The action's answer, with the renewed cookie when there is one.
    pub fn finish(&self, mut response: Response) -> Response {
        if let Some(cookie) = &self.renewed {
            response.headers_mut().append(SET_COOKIE, cookie.clone());
        }
        response
    }
}

/// `CoverUpgradeController.Signed` / `LyricsAdminController.Signed`: `_sessions.Validate(cookie
/// ?? header)`. It slides the server-side expiry but, unlike `BrowseUser`, never re-issues the
/// cookie.
pub fn signed(state: &AppState, parts: &Parts) -> bool {
    let token =
        cookie(&parts.headers, BROWSE_COOKIE_NAME).or_else(|| header(&parts.headers, BROWSE_TOKEN_HEADER));
    state.browse_sessions.validate(token.as_deref())
}

/// An answer given instead of running the action (boxed: a `Response` is large).
pub type Rejection = Box<Response>;

/// The request's head and its whole body (the raw-body capture buffered every POST anyway).
pub async fn split(request: axum::extract::Request) -> (Parts, axum::body::Bytes) {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    (parts, bytes)
}

/// The `[FromBody]` parameter's name in every 6-B2 action (`request`), which is the key of the
/// implicit `[Required]` error on the parameter itself.
const BODY_PARAMETER: &str = "request";

/// `[FromBody] T request` under `[ApiController]`, up to the object's own validation: the
/// input formatter's media types, then the body read as JSON. Every failure is the automatic
/// answer ASP.NET gave before the action ran: `415` for a body that is not JSON, and a
/// `ValidationProblemDetails` 400 for an empty body, a `null`, or JSON that does not parse.
pub fn json_body(parts: &Parts, body: &[u8]) -> Result<Value, Rejection> {
    if !is_json_content_type(&parts.headers) {
        return Err(Box::new(problem(StatusCode::UNSUPPORTED_MEDIA_TYPE)));
    }
    let required = format!("The {BODY_PARAMETER} field is required.");
    if body.is_empty() {
        return Err(Box::new(validation_problem(&[
            ("", &["A non-empty request body is required."]),
            (BODY_PARAMETER, &[required.as_str()]),
        ])));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Null) => Err(Box::new(validation_problem(&[
            ("", &["A non-empty request body is required."]),
            (BODY_PARAMETER, &[required.as_str()]),
        ]))),
        Ok(value) => Ok(value),
        Err(e) => {
            // System.Text.Json's own messages are not reproduced word for word (known-diffs.md);
            // the shape, the "$" key and the 0-based position are.
            let message = format!(
                "The JSON value is not valid. Path: $ | LineNumber: {} | BytePositionInLine: {}.",
                e.line().saturating_sub(1),
                e.column().saturating_sub(1)
            );
            Err(Box::new(validation_problem(&[
                ("$", &[message.as_str()]),
                (BODY_PARAMETER, &[required.as_str()]),
            ])))
        }
    }
}

/// [`json_body`], then the value bound to `T` with the Web defaults' leniency. A value that
/// does not fit `T` is the converter's 400, as System.Text.Json's `JsonException` was.
pub fn bind_body<T: DeserializeOwned>(parts: &Parts, body: &[u8], type_name: &str) -> Result<T, Rejection> {
    let value = json_body(parts, body)?;
    octo_core::json::web::from_value::<T>(&value).map_err(|_| {
        let required = format!("The {BODY_PARAMETER} field is required.");
        let message =
            format!("The JSON value could not be converted to {type_name}. Path: $ | LineNumber: 0 | BytePositionInLine: 0.");
        Box::new(validation_problem(&[("$", &[message.as_str()]), (BODY_PARAMETER, &[required.as_str()])]))
    })
}

/// `SystemTextJsonInputFormatter`'s media types: `application/json`, `text/json` and
/// `application/*+json`.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let media = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    media == "application/json"
        || media == "text/json"
        || (media.starts_with("application/") && media.ends_with("+json"))
}

/// The implicit `[Required]` on a non-nullable string property: null, empty and whitespace all
/// fail (`AllowEmptyStrings` is false).
pub fn required(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// `Enum.TryParse<T>(text, ignoreCase: true, out var value)`: a member name in any case, a
/// number, or several of either joined with commas (OR'ed together). `members` are the names in
/// the order of their values 0, 1, 2, ... A value no member has fails here (the C# kept the
/// undefined number; see known-diffs.md).
pub fn enum_try_parse(members: &[&str], text: Option<&str>) -> Option<usize> {
    let text = text?;
    let mut value: i64 = 0;
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        if let Some(i) = members.iter().position(|m| eq_ignore_case(m, part)) {
            value |= i as i64;
        } else if let Ok(n) = part.parse::<i64>() {
            value |= n;
        } else {
            return None;
        }
    }
    usize::try_from(value).ok().filter(|v| *v < members.len())
}

/// A C# `DateTime` (kind Utc) as STJ writes it in an API answer.
pub fn utc(value: &DateTime<Utc>) -> Value {
    Value::String(format_utc(value))
}

/// A C# `DateTime?` as STJ writes it.
pub fn utc_opt(value: &Option<DateTime<Utc>>) -> Value {
    value.as_ref().map_or(Value::Null, utc)
}

/// What System.Text.Json made of an anonymous type: its members under the camelCase policy, in
/// order. Two members whose names meet under the policy cannot both be written, and the
/// serializer threw `InvalidOperationException` before writing a byte, which the exception
/// handler answered as a 400 "Operation not valid".
pub fn anonymous(members: Vec<(&str, Value)>) -> Result<Value, AppError> {
    let mut map = serde_json::Map::with_capacity(members.len());
    for (name, value) in members {
        if map.contains_key(name) {
            return Err(AppError::InvalidOperation(format!(
                "The JSON property name for '{name}' collides with another property."
            )));
        }
        map.insert(name.to_string(), value);
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_try_parse_reads_names_numbers_and_combinations() {
        let members = ["OctoDownloads", "WholeLibrary"];
        let cases: [(Option<&str>, Option<usize>); 9] = [
            (None, None),
            (Some("wholelibrary"), Some(1)),
            (Some(" OctoDownloads "), Some(0)),
            (Some("1"), Some(1)),
            (Some("5"), None),
            (Some("OctoDownloads, WholeLibrary"), Some(1)),
            (Some(""), None),
            (Some("nope"), None),
            (Some("-1"), None),
        ];
        for (text, expected) in cases {
            assert_eq!(enum_try_parse(&members, text), expected, "case {text:?}");
        }
    }

    #[test]
    fn cookies_are_read_first_wins_and_decoded() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("a=1; octo_browse=AB%2BC; octo_browse=later"),
        );
        assert_eq!(cookie(&headers, BROWSE_COOKIE_NAME).as_deref(), Some("AB+C"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn the_cookie_is_written_as_aspnet_writes_it() {
        assert_eq!(
            browse_cookie("ABC", false).to_str().unwrap(),
            "octo_browse=ABC; max-age=7776000; path=/api/admin; samesite=strict; httponly"
        );
        assert_eq!(
            browse_cookie("ABC", true).to_str().unwrap(),
            "octo_browse=ABC; max-age=7776000; path=/api/admin; secure; samesite=strict; httponly"
        );
    }

    #[test]
    fn colliding_members_are_the_serializers_exception() {
        assert!(anonymous(vec![("busy", Value::from(1)), ("busy", Value::from(true))]).is_err());
        assert!(anonymous(vec![("a", Value::from(1)), ("b", Value::from(true))]).is_ok());
    }
}
