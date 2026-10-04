//! The admin UI and logo files: what `MapStaticAssets()` served from `wwwroot/` (which the
//! csproj also filled with a copy of `Assets/`), reproduced header for header (endpoints.md §7):
//!
//! - every file under the web root at its relative path (`/admin/index.html`), and every file
//!   under the assets directory at `/Assets/...`;
//! - matched as routes, so case and a trailing slash don't matter and a method other than GET
//!   or HEAD falls through to the catch-all (which answers 404 for these Octo-owned paths);
//! - `Content-Type` without a charset, `Accept-Ranges: bytes`, `Cache-Control: no-cache`,
//!   `ETag: "<base64 SHA-256 of the body>"` and `Last-Modified` (the file's modification time);
//! - `If-Match` / `If-Unmodified-Since` → 412, `If-None-Match` (weak comparison against the
//!   served variant's first ETag) / `If-Modified-Since` → 304, `Range` (one range, any unit
//!   name, `If-Range` honoured) → 206 or 416;
//! - Brotli and gzip variants of the text files, compressed once at startup, served when
//!   `Accept-Encoding` allows (highest q wins, Brotli on a tie), with `Content-Encoding`,
//!   `Vary: Content-Encoding`, and two ETags: the compressed body's, then `W/` the original's.
//!
//! The files are read into memory at startup: the whole UI is well under a few megabytes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::routes::RouteSet;

/// Overrides the web root (the directory holding `admin/`).
pub const WWWROOT_ENV: &str = "OCTO_WWWROOT";
/// Overrides the assets directory (the one holding `octo_logo.png`).
pub const ASSETS_ENV: &str = "OCTO_ASSETS";

/// Where the static files come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticRoots {
    pub wwwroot: Option<PathBuf>,
    pub assets: Option<PathBuf>,
}

impl StaticRoots {
    /// `OCTO_WWWROOT` / `OCTO_ASSETS` when set; else `wwwroot/` and `Assets/` beside the
    /// executable (the C# image's `AppContext.BaseDirectory` layout); else, in debug builds, the
    /// C# project's own `octo/wwwroot` and `octo/Assets` in this repository.
    pub fn from_env() -> StaticRoots {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let repo = cfg!(debug_assertions).then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../octo"));
        Self::resolve(
            std::env::var(WWWROOT_ENV).ok(),
            std::env::var(ASSETS_ENV).ok(),
            exe_dir.as_deref(),
            repo.as_deref(),
        )
    }

    /// [`StaticRoots::from_env`] over explicit inputs. A candidate counts only when it is an
    /// existing directory; an override that does not exist is used anyway (and serves nothing),
    /// so a typo shows up as missing files rather than silently serving something else.
    pub fn resolve(
        wwwroot_env: Option<String>,
        assets_env: Option<String>,
        exe_dir: Option<&Path>,
        repo: Option<&Path>,
    ) -> StaticRoots {
        let pick = |env: Option<String>, name: &str| -> Option<PathBuf> {
            if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
                return Some(PathBuf::from(v));
            }
            [exe_dir, repo]
                .into_iter()
                .flatten()
                .map(|d| d.join(name))
                .find(|p| p.is_dir())
        };
        StaticRoots {
            wwwroot: pick(wwwroot_env, "wwwroot"),
            assets: pick(assets_env, "Assets"),
        }
    }
}

/// A content coding a precompressed variant was made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Br,
    Gzip,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Br => "br",
            Encoding::Gzip => "gzip",
        }
    }
}

#[derive(Debug, Clone)]
struct Variant {
    body: Bytes,
    /// `"<base64 SHA-256 of body>"`, quotes included.
    etag: String,
}

impl Variant {
    fn new(body: Vec<u8>) -> Variant {
        let digest = Sha256::digest(&body);
        let etag = format!("\"{}\"", base64::engine::general_purpose::STANDARD.encode(digest));
        Variant {
            body: Bytes::from(body),
            etag,
        }
    }
}

/// One servable file.
#[derive(Debug, Clone)]
pub struct StaticAsset {
    /// The URL path, as the route is registered (`/admin/index.html`).
    pub url_path: String,
    pub content_type: &'static str,
    /// Truncated to whole seconds, as an HTTP date carries it.
    last_modified: SystemTime,
    last_modified_text: String,
    identity: Variant,
    /// Whether the type is worth compressing.
    compressible: bool,
    /// In preference order for a tie: Brotli, then gzip. Filled by [`StaticAsset::warm`], which
    /// the host runs in the background once it is listening: compressing the dashboard at
    /// quality 11 takes most of a second, too long to hold up the first answer. Until then the
    /// plain body is served.
    compressed: OnceLock<Vec<(Encoding, Variant)>>,
}

impl StaticAsset {
    /// Builds an asset from its bytes, compressing it when its type is compressible.
    pub fn new(
        url_path: String,
        content_type: &'static str,
        body: Vec<u8>,
        modified: SystemTime,
    ) -> StaticAsset {
        let secs = modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last_modified = UNIX_EPOCH + Duration::from_secs(secs);
        StaticAsset {
            url_path,
            content_type,
            last_modified,
            last_modified_text: httpdate::fmt_http_date(last_modified),
            identity: Variant::new(body),
            compressible: is_compressible(content_type),
            compressed: OnceLock::new(),
        }
    }

    /// Computes the compressed variants, once. Blocking; run it off the async threads.
    pub fn warm(&self) {
        self.compressed.get_or_init(|| {
            if !self.compressible {
                return Vec::new();
            }
            let body = &self.identity.body;
            vec![
                (Encoding::Br, Variant::new(brotli_compress(body))),
                (Encoding::Gzip, Variant::new(gzip_compress(body))),
            ]
        });
    }

    /// The compressed variants computed so far (none before [`StaticAsset::warm`]).
    fn compressed(&self) -> &[(Encoding, Variant)] {
        self.compressed.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The ETag of the uncompressed body.
    pub fn etag(&self) -> &str {
        &self.identity.etag
    }

    /// The ETag of a precompressed variant, when there is one.
    pub fn compressed_etag(&self, encoding: Encoding) -> Option<&str> {
        self.compressed()
            .iter()
            .find(|(e, _)| *e == encoding)
            .map(|(_, v)| v.etag.as_str())
    }
}

/// Every static file, loaded and compressed.
#[derive(Debug, Clone, Default)]
pub struct StaticAssets {
    assets: Vec<Arc<StaticAsset>>,
}

impl StaticAssets {
    /// Reads every file under the roots. Blocking: call it from `spawn_blocking` or before the
    /// runtime is busy.
    pub fn load(roots: &StaticRoots) -> StaticAssets {
        let mut assets = StaticAssets::default();
        if let Some(dir) = &roots.wwwroot {
            assets.add_dir(dir, "");
        }
        if let Some(dir) = &roots.assets {
            assets.add_dir(dir, "/Assets");
        }
        assets
    }

    /// Adds every file under `dir`, served at `prefix` + its relative path. A URL already taken
    /// keeps its first file.
    pub fn add_dir(&mut self, dir: &Path, prefix: &str) {
        if !dir.is_dir() {
            warn!(
                "Static file directory {} does not exist; nothing is served from it",
                dir.display()
            );
            return;
        }
        let mut files = Vec::new();
        collect_files(dir, &mut files);
        files.sort();
        for path in files {
            let Ok(rel) = path.strip_prefix(dir) else { continue };
            let segments: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if segments.iter().any(|s| !is_plain_segment(s)) {
                debug!("Not serving {}: its name needs URL encoding", path.display());
                continue;
            }
            let url_path = format!("{prefix}/{}", segments.join("/"));
            let Some(content_type) = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(content_type_for)
            else {
                // UseStaticFiles serves no file whose type it does not know.
                debug!("Not serving {}: unknown content type", path.display());
                continue;
            };
            if self
                .assets
                .iter()
                .any(|a| a.url_path.eq_ignore_ascii_case(&url_path))
            {
                continue;
            }
            let (body, modified) =
                match std::fs::read(&path).and_then(|b| Ok((b, std::fs::metadata(&path)?.modified()?))) {
                    Ok(x) => x,
                    Err(e) => {
                        warn!("Could not read {}: {e}", path.display());
                        continue;
                    }
                };
            self.assets
                .push(Arc::new(StaticAsset::new(url_path, content_type, body, modified)));
        }
    }

    pub fn push(&mut self, asset: StaticAsset) {
        self.assets.push(Arc::new(asset));
    }

    /// Compresses every file's variants; see [`StaticAsset::warm`]. Blocking.
    pub fn warm_all(&self) {
        for asset in &self.assets {
            asset.warm();
        }
    }

    pub fn len(&self) -> usize {
        self.assets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    pub fn get(&self, url_path: &str) -> Option<&StaticAsset> {
        self.assets
            .iter()
            .find(|a| a.url_path == url_path)
            .map(|a| a.as_ref())
    }

    /// One GET (and so HEAD) route per file.
    pub fn routes(&self) -> RouteSet {
        let mut set = RouteSet::new();
        for asset in &self.assets {
            let asset = asset.clone();
            set = set.route_with_head(
                &asset.url_path.clone(),
                get(move |req: Request| {
                    let asset = asset.clone();
                    async move { serve(&asset, req.method(), req.headers()) }
                }),
            );
        }
        set
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// A file or directory name that can be a route literal as it stands.
fn is_plain_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// `FileExtensionContentTypeProvider`'s mapping for the types a web root holds. No charset,
/// as ASP.NET sent none.
pub fn content_type_for(extension: &str) -> Option<&'static str> {
    Some(match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "map" => "text/plain",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        "xml" => "text/xml",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "woff" => "application/font-woff",
        "woff2" => "font/woff2",
        "ttf" => "application/x-font-ttf",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// The types the static web assets build precompressed: text, not images or fonts.
fn is_compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "image/svg+xml" | "application/json" | "application/manifest+json" | "application/wasm"
        )
}

fn brotli_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 4 + 64);
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 22,
        ..Default::default()
    };
    brotli::BrotliCompress(&mut &data[..], &mut out, &params).expect("compressing into memory");
    out
}

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(data.len() / 3 + 64),
        flate2::Compression::best(),
    );
    enc.write_all(data).expect("compressing into memory");
    enc.finish().expect("compressing into memory")
}

/// Which variant `Accept-Encoding` asks for: the available coding with the highest q above 0,
/// Brotli on a tie. `*` and `identity` select nothing.
fn negotiate<'a>(asset: &'a StaticAsset, headers: &HeaderMap) -> (Option<Encoding>, &'a Variant) {
    let mut best: Option<(f32, usize)> = None;
    for value in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
    {
        for item in value.split(',') {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or("").trim();
            let mut q = 1.0f32;
            for p in parts {
                let p = p.trim();
                if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                    q = v.trim().parse().unwrap_or(0.0);
                }
            }
            if q <= 0.0 {
                continue;
            }
            let Some(index) = asset
                .compressed()
                .iter()
                .position(|(e, _)| e.name().eq_ignore_ascii_case(coding))
            else {
                continue;
            };
            let better = match best {
                None => true,
                Some((bq, bi)) => q > bq || (q == bq && index < bi),
            };
            if better {
                best = Some((q, index));
            }
        }
    }
    match best {
        Some((_, i)) => (Some(asset.compressed()[i].0), &asset.compressed()[i].1),
        None => (None, &asset.identity),
    }
}

/// The request's ETag list: `None` when the header is absent, `Some(true)` for a match.
fn etag_list_matches(
    headers: &HeaderMap,
    name: header::HeaderName,
    etag: &str,
    strong: bool,
) -> Option<bool> {
    let mut present = false;
    for value in headers.get_all(name).iter().filter_map(|v| v.to_str().ok()) {
        for tag in value.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            present = true;
            if tag == "*" || etag_matches(tag, etag, strong) {
                return Some(true);
            }
        }
    }
    present.then_some(false)
}

/// RFC 9110 ETag comparison. Weak comparison ignores `W/`; strong needs both strong and equal.
fn etag_matches(theirs: &str, ours: &str, strong: bool) -> bool {
    let (their_weak, their_tag) = split_weak(theirs);
    let (our_weak, our_tag) = split_weak(ours);
    if strong && (their_weak || our_weak) {
        return false;
    }
    their_tag == our_tag
}

fn split_weak(tag: &str) -> (bool, &str) {
    match tag.strip_prefix("W/") {
        Some(rest) => (true, rest),
        None => (false, tag),
    }
}

/// A date header, when it parses and is not in the future (ASP.NET ignored future dates).
fn date_header(headers: &HeaderMap, name: header::HeaderName) -> Option<SystemTime> {
    let date = httpdate::parse_http_date(headers.get(name)?.to_str().ok()?).ok()?;
    (date <= SystemTime::now()).then_some(date)
}

enum Precondition {
    Proceed,
    NotModified,
    Failed,
}

fn precondition(asset: &StaticAsset, etag: &str, headers: &HeaderMap) -> Precondition {
    match etag_list_matches(headers, header::IF_MATCH, etag, true) {
        Some(false) => return Precondition::Failed,
        Some(true) => {}
        None => {
            if let Some(date) = date_header(headers, header::IF_UNMODIFIED_SINCE)
                && asset.last_modified > date
            {
                return Precondition::Failed;
            }
        }
    }
    match etag_list_matches(headers, header::IF_NONE_MATCH, etag, false) {
        Some(true) => Precondition::NotModified,
        Some(false) => Precondition::Proceed,
        None => match date_header(headers, header::IF_MODIFIED_SINCE) {
            Some(date) if asset.last_modified <= date => Precondition::NotModified,
            _ => Precondition::Proceed,
        },
    }
}

/// Whether `If-Range` lets a `Range` apply: absent, or naming the current (strong) ETag, or a
/// date at or after the last modification.
fn if_range_allows(asset: &StaticAsset, etag: &str, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let value = value.trim();
    if value.starts_with('"') || value.starts_with("W/") {
        return etag_matches(value, etag, true);
    }
    match httpdate::parse_http_date(value) {
        Ok(date) => asset.last_modified <= date,
        Err(_) => false,
    }
}

enum RangeOutcome {
    /// No usable range: serve the whole body.
    Whole,
    Partial(u64, u64),
    Unsatisfiable,
}

/// `RangeHelper.ParseRange` + `NormalizeRange`: exactly one range is honoured (several, or a
/// malformed header, mean the whole body), and the unit name is not checked.
fn parse_range(value: &str, length: u64) -> RangeOutcome {
    let Some((unit, spec)) = value.split_once('=') else {
        return RangeOutcome::Whole;
    };
    let unit = unit.trim();
    if unit.is_empty() || unit.contains(char::is_whitespace) {
        return RangeOutcome::Whole;
    }
    let items: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let mut parsed = Vec::with_capacity(items.len());
    for item in &items {
        let Some((from, to)) = item.split_once('-') else {
            return RangeOutcome::Whole;
        };
        let (from, to) = (from.trim(), to.trim());
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let from = if from.is_empty() {
            None
        } else if digits(from) {
            match from.parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => return RangeOutcome::Whole,
            }
        } else {
            return RangeOutcome::Whole;
        };
        let to = if to.is_empty() {
            None
        } else if digits(to) {
            match to.parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => return RangeOutcome::Whole,
            }
        } else {
            return RangeOutcome::Whole;
        };
        match (from, to) {
            (None, None) => return RangeOutcome::Whole,
            (Some(f), Some(t)) if f > t => return RangeOutcome::Whole,
            _ => parsed.push((from, to)),
        }
    }
    if parsed.len() != 1 {
        return RangeOutcome::Whole;
    }
    match parsed[0] {
        (Some(from), to) => {
            if from >= length {
                return RangeOutcome::Unsatisfiable;
            }
            let end = match to {
                Some(t) if t < length => t,
                _ => length - 1,
            };
            RangeOutcome::Partial(from, end)
        }
        (None, Some(suffix)) => {
            if suffix == 0 || length == 0 {
                return RangeOutcome::Unsatisfiable;
            }
            let n = suffix.min(length);
            RangeOutcome::Partial(length - n, length - 1)
        }
        (None, None) => RangeOutcome::Whole,
    }
}

/// An empty body with no known length, so nothing adds a `Content-Length` (ASP.NET sent none
/// on a 304 or a HEAD).
fn unsized_empty_body() -> Body {
    Body::from_stream(futures::stream::empty::<Result<Bytes, std::io::Error>>())
}

/// Answers one request for `asset`.
pub fn serve(asset: &StaticAsset, method: &Method, headers: &HeaderMap) -> Response {
    let (encoding, variant) = negotiate(asset, headers);
    let length = variant.body.len() as u64;

    let status;
    let mut content_range = None;
    let mut body = None;
    match precondition(asset, &variant.etag, headers) {
        Precondition::Failed => return bare(StatusCode::PRECONDITION_FAILED, None),
        Precondition::NotModified => status = StatusCode::NOT_MODIFIED,
        Precondition::Proceed if method == Method::HEAD => status = StatusCode::OK,
        Precondition::Proceed => {
            let range = headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .filter(|_| if_range_allows(asset, &variant.etag, headers))
                .map(|v| parse_range(v, length))
                .unwrap_or(RangeOutcome::Whole);
            match range {
                RangeOutcome::Whole => {
                    status = StatusCode::OK;
                    body = Some(variant.body.clone());
                }
                RangeOutcome::Partial(from, to) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    content_range = Some(format!("bytes {from}-{to}/{length}"));
                    body = Some(variant.body.slice(from as usize..=to as usize));
                }
                RangeOutcome::Unsatisfiable => {
                    return bare(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        Some(format!("bytes */{length}")),
                    );
                }
            }
        }
    }

    let mut res = Response::new(body.map(Body::from).unwrap_or_else(unsized_empty_body));
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.content_type));
    // ASP.NET wrote this header twice (known-diffs.md); once is what it meant.
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(e) = encoding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(e.name()));
    }
    if let Some(range) = content_range.and_then(|r| HeaderValue::from_str(&r).ok()) {
        h.insert(header::CONTENT_RANGE, range);
    }
    if let Ok(v) = HeaderValue::from_str(&variant.etag) {
        h.append(header::ETAG, v);
    }
    if encoding.is_some()
        && let Ok(v) = HeaderValue::from_str(&format!("W/{}", asset.identity.etag))
    {
        h.append(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&asset.last_modified_text) {
        h.insert(header::LAST_MODIFIED, v);
    }
    if encoding.is_some() {
        h.insert(header::VARY, HeaderValue::from_static("Content-Encoding"));
    }
    res
}

/// A 412 or 416: only the status, `Content-Length: 0` and, for 416, `Content-Range`.
fn bare(status: StatusCode, content_range: Option<String>) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = status;
    if let Some(v) = content_range.and_then(|r| HeaderValue::from_str(&r).ok()) {
        res.headers_mut().insert(header::CONTENT_RANGE, v);
    }
    res.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_normalise_as_asp_net_did() {
        let cases: [(&str, u64, Option<(u64, u64)>, bool); 10] = [
            ("bytes=0-9", 100, Some((0, 9)), false),
            ("bytes=90-200", 100, Some((90, 99)), false),
            ("bytes=95-", 100, Some((95, 99)), false),
            ("bytes=-5", 100, Some((95, 99)), false),
            ("bytes=-500", 100, Some((0, 99)), false),
            ("items=0-1", 100, Some((0, 1)), false),
            ("bytes=100-", 100, None, true),
            ("bytes=0-1,5-6", 100, None, false),
            ("bytes=9-1", 100, None, false),
            ("bytes=abc", 100, None, false),
        ];
        for (value, len, expected, unsatisfiable) in cases {
            match parse_range(value, len) {
                RangeOutcome::Partial(a, b) => assert_eq!(Some((a, b)), expected, "{value}"),
                RangeOutcome::Whole => assert!(expected.is_none() && !unsatisfiable, "{value}: whole"),
                RangeOutcome::Unsatisfiable => assert!(unsatisfiable, "{value}: unsatisfiable"),
            }
        }
    }

    #[test]
    fn roots_prefer_the_override_then_the_executable_then_the_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = tmp.path().join("bin");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(exe.join("Assets")).expect("mkdir");
        std::fs::create_dir_all(repo.join("wwwroot")).expect("mkdir");
        std::fs::create_dir_all(repo.join("Assets")).expect("mkdir");
        let r = StaticRoots::resolve(None, None, Some(&exe), Some(&repo));
        assert_eq!(r.wwwroot, Some(repo.join("wwwroot")));
        assert_eq!(r.assets, Some(exe.join("Assets")));
        let r = StaticRoots::resolve(Some("/x/www".into()), Some(" ".into()), Some(&exe), None);
        assert_eq!(r.wwwroot, Some(PathBuf::from("/x/www")));
        assert_eq!(r.assets, Some(exe.join("Assets")));
        assert_eq!(
            StaticRoots::resolve(None, None, None, None),
            StaticRoots::default()
        );
    }

    #[test]
    fn compressed_variants_decompress_to_the_original() {
        use std::io::Read;
        let body = "body { color: red; }\n".repeat(200).into_bytes();
        let a = StaticAsset::new("/x.css".into(), "text/css", body.clone(), SystemTime::now());
        assert!(a.compressed().is_empty(), "nothing is compressed before warm()");
        a.warm();
        let br = &a.compressed()[0].1.body;
        let mut out = Vec::new();
        brotli::Decompressor::new(&br[..], 4096)
            .read_to_end(&mut out)
            .expect("brotli");
        assert_eq!(out, body);
        let gz = &a.compressed()[1].1.body;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut out)
            .expect("gzip");
        assert_eq!(out, body);
        let png = StaticAsset::new("/x.png".into(), "image/png", vec![1, 2, 3], SystemTime::now());
        png.warm();
        assert!(png.compressed().is_empty());
    }
}
