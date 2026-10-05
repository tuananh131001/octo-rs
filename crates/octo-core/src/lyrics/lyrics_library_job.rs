//! The data half of `Services/Lyrics/LyricsLibraryJob.cs`: the run `lyrics-library.json` holds
//! (`LyricsLibraryRun`, its rows and review entries), its status and mode, and the request the
//! dashboard queues. The store and the worker are `octo::services::lyrics::lyrics_library_job`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};

use super::lyrics_models::LyricsResult;
use crate::models::null_as_default;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum LyricsLibraryStatus {
    #[default]
    Idle = 0,
    Running = 1,
    Completed = 2,
    Cancelled = 3,
    Interrupted = 4,
    Failed = 5,
}

/// What a run does. Walk looks every song up and saves what it finds in one pass. The dashboard's
/// lyrics page goes in steps instead, as the soft covers wall does: Scan reads the songs and
/// lists the ones with no lyrics or weaker ones, changing nothing; Preview looks up the picked
/// songs, changing nothing; Save writes what Preview found for the picked songs; Undo puts back
/// everything Save wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum LyricsLibraryMode {
    #[default]
    Walk = 0,
    Scan = 1,
    Preview = 2,
    Save = 3,
    Undo = 4,
}

/// The run's `Scope` when it walks only the songs Octo downloaded.
pub const OCTO_DOWNLOADS: &str = "OctoDownloads";

/// The run's `Scope` when it walks every song under the music folder.
pub const WHOLE_LIBRARY: &str = "WholeLibrary";

/// One song on the lyrics page's list, and what each step made of it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LyricsLibraryRow {
    // The non-nullable C# strings and lists read a JSON null as empty.
    #[serde(deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,

    /// What the song has now: none, plain or line.
    #[serde(deserialize_with = "null_as_default")]
    pub has: String,

    /// weak (listed by a scan), found, none (nothing better), busy (no service answered),
    /// saved, kept (nothing to change by the time it was saved), blocked (only lyrics Octo may not
    /// replace), failed.
    #[serde(deserialize_with = "null_as_default")]
    pub result: String,

    pub source: Option<String>,
    pub kind: Option<String>,
    pub candidate_id: Option<String>,
    pub doubt: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub preview: Vec<String>,

    /// The lyrics a preview found, kept for Save; never sent to the dashboard.
    pub found_synced: Option<String>,
    pub found_plain: Option<String>,
}

impl Default for LyricsLibraryRow {
    fn default() -> Self {
        Self {
            id: String::new(),
            path: String::new(),
            artist: String::new(),
            title: String::new(),
            album: None,
            has: "none".to_string(),
            result: "weak".to_string(),
            source: None,
            kind: None,
            candidate_id: None,
            doubt: None,
            preview: Vec::new(),
            found_synced: None,
            found_plain: None,
        }
    }
}

impl LyricsLibraryRow {
    /// The lyrics a preview found, as the writer takes them; None before a preview found any.
    /// (The C# computed property `Found`, which System.Text.Json also wrote.)
    pub fn found(&self) -> Option<LyricsResult> {
        if self.found_synced.is_none() && self.found_plain.is_none() {
            return None;
        }
        let mut found = LyricsResult::new(
            self.source.clone().unwrap_or_else(|| "found".to_string()),
            self.found_synced.clone(),
            self.found_plain.clone(),
            false,
        );
        found.candidate_id = self.candidate_id.clone();
        found.doubt = self.doubt.clone();
        Some(found)
    }

    /// A short id for a file, the same on every scan: the first 16 hex digits, lower case, of
    /// the SHA-1 of its path in UTF-8.
    pub fn id_of(path: &str) -> String {
        hex::encode(&sha1(path.as_bytes())[..8])
    }
}

/// The shape System.Text.Json wrote: the settable properties, then the computed `Found`.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricsLibraryRowOut<'a> {
    id: &'a str,
    path: &'a str,
    artist: &'a str,
    title: &'a str,
    album: &'a Option<String>,
    has: &'a str,
    result: &'a str,
    source: &'a Option<String>,
    kind: &'a Option<String>,
    candidate_id: &'a Option<String>,
    doubt: &'a Option<String>,
    preview: &'a [String],
    found_synced: &'a Option<String>,
    found_plain: &'a Option<String>,
    found: Option<LyricsResult>,
}

impl Serialize for LyricsLibraryRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        LyricsLibraryRowOut {
            id: &self.id,
            path: &self.path,
            artist: &self.artist,
            title: &self.title,
            album: &self.album,
            has: &self.has,
            result: &self.result,
            source: &self.source,
            kind: &self.kind,
            candidate_id: &self.candidate_id,
            doubt: &self.doubt,
            preview: &self.preview,
            found_synced: &self.found_synced,
            found_plain: &self.found_plain,
            found: self.found(),
        }
        .serialize(serializer)
    }
}

/// A lyric the job wrote but is not sure of, for someone to look at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LyricsReviewEntry {
    #[serde(deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    #[serde(deserialize_with = "null_as_default")]
    pub kind: String,
    pub candidate_id: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub reason: String,
    #[serde(with = "crate::json::datetime::utc")]
    pub at_utc: DateTime<Utc>,
}

impl Default for LyricsReviewEntry {
    /// What a positional record read from JSON got for a missing member: its type's default.
    fn default() -> Self {
        Self {
            path: String::new(),
            artist: String::new(),
            title: String::new(),
            album: None,
            duration_seconds: None,
            source: String::new(),
            kind: String::new(),
            candidate_id: None,
            reason: String::new(),
            at_utc: crate::json::datetime::min_value(),
        }
    }
}

/// The run `lyrics-library.json` holds, with the computed `CanResume` written last.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LyricsLibraryRun {
    #[serde(deserialize_with = "null_as_default")]
    pub run_id: String,
    pub status: LyricsLibraryStatus,

    /// OctoDownloads, or WholeLibrary when "Write lyrics files beside all library songs" was on
    /// when the run started.
    #[serde(deserialize_with = "null_as_default")]
    pub scope: String,

    /// Also look again where a song's lyrics are weaker than the sources would choose now:
    /// plain, or line-timed while word timing is preferred.
    pub upgrade: bool,

    pub mode: LyricsLibraryMode,

    /// The songs a scan listed, with what Preview and Save made of them.
    #[serde(deserialize_with = "null_as_default")]
    pub rows: Vec<LyricsLibraryRow>,

    /// The rows picked for Preview or Save; None for a scan or a walk.
    pub picked: Option<Vec<String>>,

    /// Songs a scan found with word-timed lyrics already.
    pub word_already: i32,

    #[serde(with = "crate::json::datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,
    #[serde(with = "crate::json::datetime::utc_option")]
    pub finished_utc: Option<DateTime<Utc>>,
    pub total: i32,
    pub processed: i32,
    pub written: i32,
    pub word_timed: i32,
    pub upgraded: i32,
    pub already_had: i32,
    pub not_found: i32,
    pub instrumental: i32,
    pub busy: i32,
    pub skipped: i32,
    pub failed: i32,
    pub cursor: i32,
    pub last_path: Option<String>,
    pub reason: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub errors: Vec<String>,

    /// The files this run walks, kept so a resume carries on in the same order.
    #[serde(deserialize_with = "null_as_default")]
    pub queue: Vec<String>,

    #[serde(deserialize_with = "null_as_default")]
    pub review: Vec<LyricsReviewEntry>,
}

impl Default for LyricsLibraryRun {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            status: LyricsLibraryStatus::Idle,
            scope: OCTO_DOWNLOADS.to_string(),
            upgrade: false,
            mode: LyricsLibraryMode::Walk,
            rows: Vec::new(),
            picked: None,
            word_already: 0,
            started_utc: None,
            finished_utc: None,
            total: 0,
            processed: 0,
            written: 0,
            word_timed: 0,
            upgraded: 0,
            already_had: 0,
            not_found: 0,
            instrumental: 0,
            busy: 0,
            skipped: 0,
            failed: 0,
            cursor: 0,
            last_path: None,
            reason: None,
            errors: Vec::new(),
            queue: Vec::new(),
            review: Vec::new(),
        }
    }
}

impl LyricsLibraryRun {
    /// A stopped or interrupted run with songs left to do; an Undo is never resumed.
    pub fn can_resume(&self) -> bool {
        matches!(
            self.status,
            LyricsLibraryStatus::Cancelled | LyricsLibraryStatus::Interrupted
        ) && self.mode != LyricsLibraryMode::Undo
            && i64::from(self.cursor) < self.queue.len() as i64
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricsLibraryRunOut<'a> {
    run_id: &'a str,
    status: LyricsLibraryStatus,
    scope: &'a str,
    upgrade: bool,
    mode: LyricsLibraryMode,
    rows: &'a [LyricsLibraryRow],
    picked: &'a Option<Vec<String>>,
    word_already: i32,
    #[serde(serialize_with = "utc_option")]
    started_utc: &'a Option<DateTime<Utc>>,
    #[serde(serialize_with = "utc_option")]
    finished_utc: &'a Option<DateTime<Utc>>,
    total: i32,
    processed: i32,
    written: i32,
    word_timed: i32,
    upgraded: i32,
    already_had: i32,
    not_found: i32,
    instrumental: i32,
    busy: i32,
    skipped: i32,
    failed: i32,
    cursor: i32,
    last_path: &'a Option<String>,
    reason: &'a Option<String>,
    errors: &'a [String],
    queue: &'a [String],
    review: &'a [LyricsReviewEntry],
    can_resume: bool,
}

fn utc_option<S: Serializer>(value: &&Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error> {
    crate::json::datetime::utc_option::serialize(value, serializer)
}

impl Serialize for LyricsLibraryRun {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        LyricsLibraryRunOut {
            run_id: &self.run_id,
            status: self.status,
            scope: &self.scope,
            upgrade: self.upgrade,
            mode: self.mode,
            rows: &self.rows,
            picked: &self.picked,
            word_already: self.word_already,
            started_utc: &self.started_utc,
            finished_utc: &self.finished_utc,
            total: self.total,
            processed: self.processed,
            written: self.written,
            word_timed: self.word_timed,
            upgraded: self.upgraded,
            already_had: self.already_had,
            not_found: self.not_found,
            instrumental: self.instrumental,
            busy: self.busy,
            skipped: self.skipped,
            failed: self.failed,
            cursor: self.cursor,
            last_path: &self.last_path,
            reason: &self.reason,
            errors: &self.errors,
            queue: &self.queue,
            review: &self.review,
            can_resume: self.can_resume(),
        }
        .serialize(serializer)
    }
}

/// What the dashboard asks the job to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsLibraryRequest {
    pub upgrade: bool,
    pub resume: bool,
    pub mode: LyricsLibraryMode,
    pub scope: Option<String>,
    pub picked: Option<Vec<String>>,
}

impl LyricsLibraryRequest {
    /// `new LyricsLibraryRequest(upgrade)`: a walk.
    pub fn new(upgrade: bool) -> Self {
        Self {
            upgrade,
            ..Self::default()
        }
    }
}

/// SHA-1 (FIPS 180-4), for [`LyricsLibraryRow::id_of`]. Small enough to carry here rather than
/// add a crate for one short id.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut message = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(value);
        }
    }
    let mut digest = [0u8; 20];
    for (chunk, value) in digest.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lyrics-library.json"
        );
        std::fs::read_to_string(path).expect("the fixture is in the repo")
    }

    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let run: LyricsLibraryRun = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(run.status, LyricsLibraryStatus::Completed);
        assert_eq!(run.mode, LyricsLibraryMode::Preview);
        assert_eq!(run.scope, WHOLE_LIBRARY);
        assert!(!run.can_resume());
        assert_eq!(
            run.rows[0].found().map(|found| found.source),
            Some("kugou".to_string())
        );
        assert_eq!(run.rows[1].found(), None);
        assert_eq!(run.review[0].duration_seconds, Some(257));
        assert_eq!(crate::json::to_string(&run), text.trim_end_matches('\n'));
    }

    /// The row ids in the fixture are the C#'s own, so they pin the hash down.
    #[test]
    fn id_of_is_the_first_sixteen_hex_digits_of_the_paths_sha1() {
        let run: LyricsLibraryRun = serde_json::from_str(&fixture()).expect("the fixture reads");
        for row in &run.rows {
            assert_eq!(LyricsLibraryRow::id_of(&row.path), row.id, "{}", row.path);
        }
        // FIPS 180-4's own examples, the second one two blocks long.
        assert_eq!(
            hex::encode(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex::encode(sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(hex::encode(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn can_resume_needs_a_stopped_run_with_songs_left_that_is_not_an_undo() {
        let mut run = LyricsLibraryRun {
            status: LyricsLibraryStatus::Interrupted,
            queue: vec!["a".into(), "b".into()],
            cursor: 1,
            ..Default::default()
        };
        assert!(run.can_resume());
        run.mode = LyricsLibraryMode::Undo;
        assert!(!run.can_resume());
        run.mode = LyricsLibraryMode::Save;
        run.cursor = 2;
        assert!(!run.can_resume());
        run.cursor = 0;
        run.status = LyricsLibraryStatus::Completed;
        assert!(!run.can_resume());
        run.status = LyricsLibraryStatus::Cancelled;
        assert!(run.can_resume());
    }

    /// A row's Found carries its source (or "found"), the candidate and the doubt, and is
    /// written but never read back.
    #[test]
    fn found_is_computed_written_and_ignored_on_read() {
        let row = LyricsLibraryRow {
            found_plain: Some("words".into()),
            candidate_id: Some("lrclib:1".into()),
            doubt: Some("why".into()),
            ..Default::default()
        };
        let found = row.found().expect("found");
        assert_eq!(found.source, "found");
        assert_eq!(found.candidate_id.as_deref(), Some("lrclib:1"));
        assert_eq!(found.doubt.as_deref(), Some("why"));

        let json = crate::json::to_string(&row);
        assert!(json.contains("\"Found\":{\"Source\":\"found\""), "{json}");
        let back: LyricsLibraryRow = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, row);

        // The defaults of a fresh row and run, and nulls where C# declared non-nullable.
        let read: LyricsLibraryRun =
            serde_json::from_str(r#"{"Rows":[{"Path":null}],"Errors":null}"#).expect("reads");
        assert_eq!(read.scope, OCTO_DOWNLOADS);
        assert_eq!(read.rows[0].has, "none");
        assert_eq!(read.rows[0].result, "weak");
        assert_eq!(read.rows[0].path, "");
        assert!(read.errors.is_empty());
    }
}
