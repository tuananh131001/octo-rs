//! The pure half of `Services/Soulseek/SoulseekClient.cs`: the records slskd's answers become,
//! the request bodies, and the readers of slskd's JSON. The HTTP client itself (auth, the
//! operation gate, the search and transfer waits) is `octo::services::soulseek::soulseek_client`.
//!
//! The readers keep `JsonDocument`'s habit of throwing where serde_json would quietly answer
//! `None` (`GetString()` on a number, `GetInt32()` on `3.5`, `TryGetProperty` on an array), so
//! they return an [`ElementResult`] at the places the C# could throw, and their callers catch
//! what the C# caught.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::common::dotnet;
use crate::json::element::{ElementResult, get_boolean, get_int32, get_int64, get_string, try_get_property};
use crate::soulseek::search_profile::SearchProfile;
use crate::soulseek::soulseek_link::{SoulseekLinkState, SoulseekServerReading};

/// How many times a POST slskd refused with 429 is sent again, waiting
/// SearchStartRetryDelay longer each time.
pub const OPERATION_RETRIES: u32 = 3;

/// The longest a transfer that keeps moving is waited for. A slow peer with the right
/// file is worth waiting on; one that trickles for an hour is not.
pub const MAX_TRANSFER_TIME: Duration = Duration::from_secs(60 * 60);

/// How long a cancelled search is given to end and save its responses. slskd takes
/// well under a second; the rest is slack for a busy disk.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// After we've seen the transfer at least once, missing it for this many
/// consecutive polls means slskd dropped it and we should give up. Some
/// slskd versions remove rejected transfers from the active-list endpoint
/// immediately, so without this we'd poll forever.
pub const MAX_CONSECUTIVE_MISSES_AFTER_SEEN: i32 = 6; // ~9s at 1500ms cadence

/// One file a peer offers, from a search answer or a folder listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SoulseekFileHit {
    pub username: String,
    pub filename: String,
    pub size: i64,
    pub bit_rate: Option<i32>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub length: Option<i32>,
    pub extension: String,
    pub upload_speed: Option<i32>,
    pub queue_length: Option<i32>,

    /// The peer can start sending now rather than queueing us. Per response, like
    /// QueueLength, so every file one peer offers carries the same value.
    pub has_free_upload_slot: Option<bool>,
}

/// How a transfer ended, as far as Octo is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoulseekTransferState {
    Succeeded,
    Errored,
}

/// One poll's view of a transfer. PercentComplete is slskd's own, from 0 to 100.
#[derive(Debug, Clone, PartialEq)]
pub struct SoulseekTransferProgress {
    pub state: String,
    pub bytes_transferred: Option<i64>,
    pub size: Option<i64>,
    pub percent_complete: Option<f64>,
}

impl SoulseekTransferProgress {
    /// Bytes are flowing, or have. A transfer waiting in the peer's queue reads "Queued,
    /// Remotely" with nothing moved, and that is still a wait, not a download.
    pub fn is_moving(&self) -> bool {
        self.bytes_transferred.is_some_and(|b| b > 0)
            || contains_ignore_case(&self.state, "InProgress")
            || contains_ignore_case(&self.state, "Succeeded")
    }
}

/// How long to keep waiting on one transfer. Each time more bytes have arrived, the
/// quiet window starts again, so a slow peer that keeps sending is waited for. A
/// transfer with nothing new for the whole window, or still unfinished at the ceiling,
/// is given up on.
#[derive(Debug, Clone)]
pub struct TransferWatch {
    started: DateTime<Utc>,
    quiet: TimeDelta,
    ceiling: TimeDelta,
    quiet_since: DateTime<Utc>,
    bytes: i64,
}

impl TransferWatch {
    pub fn new(started: DateTime<Utc>, quiet: Duration, ceiling: Duration) -> Self {
        TransferWatch {
            started,
            quiet: delta(quiet),
            ceiling: delta(ceiling),
            quiet_since: started,
            bytes: 0,
        }
    }

    pub fn saw(&mut self, bytes: Option<i64>, now: DateTime<Utc>) {
        let Some(b) = bytes else { return };
        if b <= self.bytes {
            return;
        }
        self.bytes = b;
        self.quiet_since = now;
    }

    pub fn hit_ceiling(&self, now: DateTime<Utc>) -> bool {
        now - self.started >= self.ceiling
    }

    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        now - self.quiet_since >= self.quiet || self.hit_ceiling(now)
    }
}

/// A `TimeSpan` as a chrono span, saturating (no wait here comes near the limit).
pub fn delta(span: Duration) -> TimeDelta {
    TimeDelta::from_std(span).unwrap_or(TimeDelta::MAX)
}

/// What a batch enqueue came to. TransferIds maps each queued file to its transfer id;
/// Failures are the files slskd would not queue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatchEnqueue {
    pub supported: bool,
    pub transfer_ids: HashMap<String, String>,
    pub failures: Vec<(String, String)>,
}

impl BatchEnqueue {
    /// An slskd without batch downloads.
    pub fn not_supported() -> Self {
        BatchEnqueue::default()
    }
}

/// One read of slskd's search record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchStatus {
    pub state: String,
    pub ended: bool,
    pub response_count: i32,
}

/// `s.Contains(value, StringComparison.OrdinalIgnoreCase)`.
pub fn contains_ignore_case(s: &str, value: &str) -> bool {
    dotnet::ordinal_ignore_case_key(s).contains(&dotnet::ordinal_ignore_case_key(value))
}

/// `TryGetPropertyIgnoreCase`: the exact name first, then the first property whose name
/// matches ignoring case. Nothing for an element that is not an object.
pub fn try_get_property_ignore_case<'a>(element: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(map) = element else {
        return None;
    };
    map.get(name).or_else(|| {
        map.iter()
            .find(|(key, _)| dotnet::eq_ignore_case(key, name))
            .map(|(_, value)| value)
    })
}

/// `Path.GetExtension` on Linux: from the last `.` of the file name (after the last `/`), dot
/// included, or empty when there is none or it is the last character.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
    match name.rfind('.') {
        Some(at) if at + 1 < name.len() => &name[at..],
        _ => "",
    }
}

/// Reduce a file extension to the bare lowercase form ("flac").
///
/// Candidate ranking accepts a hit by comparing this against the configured
/// PreferredExtension, so the two have to agree on shape. slskd does not
/// guarantee one: some builds report "flac", some report ".flac", and some
/// omit the field entirely and leave only the filename to go on. An
/// unnormalized leading dot compared against a bare "flac" matches nothing,
/// which reads downstream as "this track is not on Soulseek" rather than as
/// a parsing mismatch. Both sides of the comparison run through here.
pub fn normalize_extension(extension: Option<&str>, filename: &str) -> String {
    let raw = match extension {
        Some(e) if !dotnet::is_blank(e) => e,
        _ => get_extension(filename),
    };
    dotnet::to_lower_invariant(raw.trim().trim_start_matches('.'))
}

/// Reads server.isConnected and server.isLoggedIn, exactly what slskd checks before it will
/// start a search, rather than the state words. A shape without them is Unknown, which never
/// holds anything back. address and ipEndPoint are left out while disconnected, so nothing
/// here depends on them.
pub fn parse_server_reading(json: &str) -> SoulseekServerReading {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return SoulseekServerReading::unknown();
    };
    let Some(server) = try_get_property_ignore_case(&root, "server").filter(|s| s.is_object()) else {
        return SoulseekServerReading::unknown();
    };
    let connected = flag(server, "isConnected");
    let logged_in = flag(server, "isLoggedIn");
    let link = match (connected, logged_in) {
        (Some(true), Some(true)) => SoulseekLinkState::LoggedIn,
        (Some(_), Some(_)) => SoulseekLinkState::NotLoggedIn,
        _ => SoulseekLinkState::Unknown,
    };
    let text = |element: Option<&Value>| match element {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    };
    let state = text(try_get_property_ignore_case(server, "state"));
    let username = text(
        try_get_property_ignore_case(&root, "user")
            .and_then(|user| try_get_property_ignore_case(user, "username")),
    );
    let next = text(
        try_get_property_ignore_case(&root, "connectionWatchdog")
            .and_then(|dog| try_get_property_ignore_case(dog, "nextAttemptAt")),
    )
    .and_then(|at| crate::json::datetime::parse_utc(&at));
    SoulseekServerReading::new(link, state, username, next)
}

fn flag(element: &Value, name: &str) -> Option<bool> {
    match try_get_property_ignore_case(element, name) {
        Some(Value::Bool(value)) => Some(*value),
        _ => None,
    }
}

/// Every option of `/api/v0/options` lives under `directories`; the one named here, when it
/// is a string. A null (endpoint missing, redacted, or unexpected shape) must never gate
/// anything.
pub fn parse_directory_option(json: &str, name: &str) -> Result<Option<String>, serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    if !root.is_object() {
        return Ok(None);
    }
    let Some(dirs) = try_get_property_ignore_case(&root, "directories") else {
        return Ok(None);
    };
    Ok(match try_get_property_ignore_case(dirs, name) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    })
}

/// The body of a search start. The timeout is in milliseconds, whatever slskd's own API doc
/// says: it is passed to Soulseek.NET unchanged.
pub fn search_payload(search_id: &str, query: &str, profile: &SearchProfile) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload<'a> {
        id: &'a str,
        search_text: &'a str,
        // Milliseconds, whatever slskd's own API doc says: it is passed to Soulseek.NET unchanged.
        search_timeout: i32,
        response_limit: i32,
        file_limit: i32,
        filter_responses: bool,
    }
    crate::json::to_string(&Payload {
        id: search_id,
        search_text: query,
        search_timeout: profile.search_timeout_ms,
        response_limit: profile.response_limit,
        file_limit: profile.file_limit,
        filter_responses: true,
    })
}

#[derive(Serialize)]
struct FileRequest<'a> {
    filename: &'a str,
    size: i64,
}

/// The body of a one-file enqueue: `[{ filename, size }]`.
pub fn enqueue_payload(filename: &str, size: i64) -> String {
    crate::json::to_string(&[FileRequest { filename, size }])
}

/// The body of a folder listing request: `{ directory }`.
pub fn browse_payload(directory: &str) -> String {
    #[derive(Serialize)]
    struct Payload<'a> {
        directory: &'a str,
    }
    crate::json::to_string(&Payload { directory })
}

/// The body of slskd's session login.
pub fn session_payload(username: &str, password: &str) -> String {
    #[derive(Serialize)]
    struct Payload<'a> {
        username: &'a str,
        password: &'a str,
    }
    crate::json::to_string(&Payload { username, password })
}

/// The body of a batch enqueue: one peer's files, all landing in `destination`.
pub fn batch_payload(username: &str, files: &[(String, i64)], destination: &str) -> String {
    #[derive(Serialize)]
    struct Options<'a> {
        destination: &'a str,
    }
    #[derive(Serialize)]
    struct Payload<'a> {
        id: String,
        username: &'a str,
        files: Vec<FileRequest<'a>>,
        options: Options<'a>,
    }
    crate::json::to_string(&Payload {
        id: uuid::Uuid::new_v4().to_string(),
        username,
        files: files
            .iter()
            .map(|(filename, size)| FileRequest {
                filename,
                size: *size,
            })
            .collect(),
        options: Options { destination },
    })
}

/// Reads a batch answer: batch.transfers carries what slskd queued, failures what it
/// would not. Anything unreadable reads as nothing queued. (A transfer or failure that is not an
/// object, or a name that is not a string, threw past the C#'s `catch (JsonException)`, and does
/// here too.)
pub fn parse_batch(json: &str) -> ElementResult<BatchEnqueue> {
    let mut ids = HashMap::new();
    let mut failures = Vec::new();
    if let Ok(root) = serde_json::from_str::<Value>(json) {
        if let Some(Value::Array(transfers)) = try_get_property_ignore_case(&root, "batch")
            .and_then(|batch| try_get_property_ignore_case(batch, "transfers"))
        {
            for transfer in transfers {
                if let Some(Value::String(name)) = try_get_property(transfer, "filename")?
                    && let Some(id) = transfer_id(transfer)?
                {
                    ids.insert(name.clone(), id);
                }
            }
        }
        if let Some(Value::Array(failed)) = try_get_property_ignore_case(&root, "failures") {
            for failure in failed {
                let text = |name: &str| -> ElementResult<String> {
                    Ok(match try_get_property(failure, name)? {
                        Some(value) => get_string(value)?.unwrap_or_default().to_string(),
                        None => String::new(),
                    })
                };
                failures.push((text("filename")?, text("message")?));
            }
        }
    }
    Ok(BatchEnqueue {
        supported: true,
        transfer_ids: ids,
        failures,
    })
}

/// One read of slskd's search record. Ended means endedAt is set, not that the state says
/// Completed: slskd saves Completed the moment the network search stops, and the responses a
/// moment later in the same save that sets endedAt. Reading on Completed alone can find the
/// empty list from in between.
///
/// Unparseable JSON is an error, which the caller treats as a failed read.
pub fn parse_search_status(json: &str) -> Result<Option<SearchStatus>, serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    let Value::Object(map) = &root else {
        return Ok(None);
    };
    let state = match map.get("state") {
        Some(Value::String(state)) => state.clone(),
        _ => String::new(),
    };
    let ended = matches!(map.get("endedAt"), Some(Value::String(_)));
    let responses = match map.get("responseCount") {
        Some(Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        _ => 0,
    };
    Ok(Some(SearchStatus {
        state,
        ended,
        response_count: responses,
    }))
}

/// `TryGetProperty(name) && ValueKind == Number ? GetInt32() : null`: a number that is not a
/// whole Int32 throws.
fn opt_int32(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => get_int32(value).map(Some),
        _ => Ok(None),
    }
}

fn size_of(element: &Value) -> ElementResult<i64> {
    match try_get_property(element, "size")? {
        Some(value @ Value::Number(_)) => get_int64(value),
        _ => Ok(0),
    }
}

fn opt_string<'a>(element: &'a Value, name: &str) -> ElementResult<Option<&'a str>> {
    match try_get_property(element, name)? {
        Some(value) => get_string(value),
        None => Ok(None),
    }
}

/// Every file of a search's responses. The second value is the message of whatever ended the
/// read early (bad JSON, or a field of the wrong kind), for the caller to log; the files read
/// before it are kept, as the C# kept them.
pub fn parse_responses(json: &str) -> (Vec<SoulseekFileHit>, Option<String>) {
    let mut hits = Vec::new();
    let outcome = (|| -> Result<(), String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let Value::Array(responses) = &root else {
            return Ok(());
        };
        for resp in responses {
            read_response(resp, &mut hits).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    (hits, outcome.err())
}

fn read_response(resp: &Value, hits: &mut Vec<SoulseekFileHit>) -> ElementResult<()> {
    let Some(uname) = try_get_property(resp, "username")? else {
        return Ok(());
    };
    let username = get_string(uname)?.unwrap_or_default().to_string();
    if dotnet::is_blank(&username) {
        return Ok(());
    }
    let Some(Value::Array(files)) = try_get_property(resp, "files")? else {
        return Ok(());
    };

    let upload_speed = opt_int32(resp, "uploadSpeed")?;
    let queue_length = opt_int32(resp, "queueLength")?;
    let free_slot = match try_get_property(resp, "hasFreeUploadSlot")? {
        Some(value @ Value::Bool(_)) => Some(get_boolean(value)?),
        _ => None,
    };

    for file in files {
        let Some(filename) = opt_string(file, "filename")? else {
            continue;
        };
        if dotnet::is_blank(filename) {
            continue;
        }
        let ext = opt_string(file, "extension")?;
        hits.push(SoulseekFileHit {
            username: username.clone(),
            filename: filename.to_string(),
            size: size_of(file)?,
            bit_rate: opt_int32(file, "bitRate")?,
            sample_rate: opt_int32(file, "sampleRate")?,
            bit_depth: opt_int32(file, "bitDepth")?,
            length: opt_int32(file, "length")?,
            extension: normalize_extension(ext, filename),
            upload_speed,
            queue_length,
            has_free_upload_slot: free_slot,
        });
    }
    Ok(())
}

/// Why a folder listing could not be read: JSON that does not parse (`JsonException`, which
/// the browse catches), or an element of the wrong kind (which it did not).
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Element(#[from] crate::json::element::ElementError),
}

/// slskd's answer for one folder: a folder object, or a list of them, each with its files. A
/// file's name is either its full remote path or its name alone, which is then put under the
/// folder it was listed in.
pub fn parse_directory(
    json: &str,
    from: &SoulseekFileHit,
    directory: &str,
) -> Result<Vec<SoulseekFileHit>, DirectoryError> {
    let mut hits = Vec::new();
    let root: Value = serde_json::from_str(json)?;
    let folders: Vec<&Value> = match &root {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![&root],
        _ => Vec::new(),
    };
    for folder in folders {
        let name = match try_get_property(folder, "name")? {
            Some(Value::String(name)) => Some(name.as_str()),
            _ => None,
        };
        let place = match name {
            Some(name) if !dotnet::is_blank(name) => name,
            _ => directory,
        };
        let Some(Value::Array(files)) = try_get_property(folder, "files")? else {
            continue;
        };
        for file in files {
            let Some(filename) = opt_string(file, "filename")? else {
                continue;
            };
            if dotnet::is_blank(filename) {
                continue;
            }
            let filename = if filename.contains(['\\', '/']) {
                filename.to_string()
            } else {
                format!("{}\\{filename}", place.trim_end_matches(['\\', '/']))
            };
            let ext = opt_string(file, "extension")?;
            hits.push(SoulseekFileHit {
                username: from.username.clone(),
                size: size_of(file)?,
                bit_rate: opt_int32(file, "bitRate")?,
                sample_rate: opt_int32(file, "sampleRate")?,
                bit_depth: opt_int32(file, "bitDepth")?,
                length: opt_int32(file, "length")?,
                extension: normalize_extension(ext, &filename),
                filename,
                upload_speed: from.upload_speed,
                queue_length: from.queue_length,
                has_free_upload_slot: from.has_free_upload_slot,
            });
        }
    }
    Ok(hits)
}

/// The id slskd gives a transfer, for cancelling it.
pub fn transfer_id(file: &Value) -> ElementResult<Option<String>> {
    Ok(match try_get_property(file, "id")? {
        Some(Value::String(id)) => Some(id.clone()),
        _ => None,
    })
}

/// A transfer's state words, `""` when it has none.
pub fn state_of(file: &Value) -> ElementResult<String> {
    Ok(match try_get_property(file, "state")? {
        Some(state) => get_string(state)?.unwrap_or_default().to_string(),
        None => String::new(),
    })
}

/// Finds a transfer's state in an slskd downloads response, or None when the
/// file is not present. The per-user endpoint returns a single
/// {username, directories} object, while the all-users endpoint returns an
/// array of them; both shapes are accepted. Reading the wrong shape is what
/// made every completed transfer look like a timeout: the poll loop rejected
/// the object response wholesale and rode the per-attempt timer to the end.
pub fn find_transfer_state(root: &Value, filename: &str) -> ElementResult<Option<String>> {
    match find_transfer(root, filename, None)? {
        Some(file) => state_of(file).map(Some),
        None => Ok(None),
    }
}

/// What slskd says a transfer has moved so far. Any field it leaves out, or sends as
/// something other than a number, is None rather than zero, so a missing size never reads
/// as a finished file.
pub fn read_transfer_progress(file: &Value) -> ElementResult<SoulseekTransferProgress> {
    let long = |name: &str| -> ElementResult<Option<i64>> {
        Ok(match try_get_property(file, name)? {
            Some(Value::Number(n)) => n.as_i64(),
            _ => None,
        })
    };
    let double = |name: &str| -> ElementResult<Option<f64>> {
        Ok(match try_get_property(file, name)? {
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        })
    };
    Ok(SoulseekTransferProgress {
        state: state_of(file)?,
        bytes_transferred: long("bytesTransferred")?,
        size: long("size")?,
        percent_complete: double("percentComplete")?,
    })
}

/// The file object for a transfer, in either response shape, or None. With an id, only
/// that transfer: an older transfer of the same file from the same peer may still be listed.
pub fn find_transfer<'a>(
    root: &'a Value,
    filename: &str,
    transfer: Option<&str>,
) -> ElementResult<Option<&'a Value>> {
    let user_groups: Vec<&Value> = match root {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![root],
        _ => Vec::new(),
    };

    for user_group in user_groups {
        if !user_group.is_object() {
            continue;
        }
        let Some(dirs) = try_get_property(user_group, "directories")? else {
            continue;
        };
        let Value::Array(dirs) = dirs else { continue };
        for dir in dirs {
            let Some(files) = try_get_property(dir, "files")? else {
                continue;
            };
            let Value::Array(files) = files else { continue };
            for file in files {
                if let Some(wanted) = transfer {
                    if transfer_id(file)?.as_deref() == Some(wanted) {
                        return Ok(Some(file));
                    }
                    continue;
                }
                let name = opt_string(file, "filename")?;
                if name != Some(filename) {
                    continue;
                }
                return Ok(Some(file));
            }
        }
    }
    Ok(None)
}

/// The JWT slskd's session endpoint answers with: `token` and `expires` (Unix seconds). A
/// missing property or one of the wrong kind throws, as `GetProperty` did.
pub fn parse_session(json: &str) -> anyhow::Result<(Option<String>, DateTime<Utc>)> {
    let root: Value = serde_json::from_str(json)?;
    let token = get_string(crate::json::element::get_property(&root, "token")?)?.map(str::to_string);
    let expires = get_int64(crate::json::element::get_property(&root, "expires")?)?;
    let expires = DateTime::<Utc>::from_timestamp(expires, 0).ok_or_else(|| {
        anyhow::anyhow!("Valid values are between -62135596800 and 253402300799, inclusive.")
    })?;
    Ok((token, expires))
}

#[cfg(test)]
#[path = "soulseek_client_tests.rs"]
mod tests;
