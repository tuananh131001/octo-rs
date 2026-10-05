//! Port of `Services/Lyrics/LyricsSidecarWriter.cs`: `LyricsJob`, `LyricsWriteOutcome`,
//! `LyricsWrite` and the writer, a singleton that is also a hosted queue worker.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use octo_core::common::dotnet::{eq_ignore_case, is_null_or_white_space, to_lower_invariant};
use octo_core::lyrics::song_lyrics::OCTO_MARK;
use octo_core::lyrics::{LyricsLookup, LyricsQuery, LyricsResult, LyricsTiming, SongLyrics, SongLyricsPlace};
use octo_core::settings::{LyricsSaveTo, SettingsStore};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::lyrics_service::LyricsService;
use super::lyrics_undo_journal::{LyricsUndoEntry, LyricsUndoJournal};
use super::song_lyrics;
use crate::services::local::ILocalLibraryService;

/// One song to find lyrics for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsJob {
    pub audio_path: PathBuf,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    /// 1 for the first try.
    pub attempt: i32,
}

impl LyricsJob {
    pub fn new(
        audio_path: impl Into<PathBuf>,
        artist: impl Into<String>,
        title: impl Into<String>,
        album: Option<String>,
        duration_seconds: Option<i32>,
    ) -> Self {
        Self {
            audio_path: audio_path.into(),
            artist: artist.into(),
            title: title.into(),
            album,
            duration_seconds,
            attempt: 1,
        }
    }

    /// `job with { Attempt = attempt }`.
    pub fn with_attempt(mut self, attempt: i32) -> Self {
        self.attempt = attempt;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LyricsWriteOutcome {
    Written,
    Instrumental,
    NotFound,
    AlreadyThere,
    Gone,
    Retrying,
    GaveUp,
    Upgraded,
}

/// What a write did, and the lyrics it wrote, for the library job's review list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsWrite {
    pub outcome: LyricsWriteOutcome,
    pub result: Option<LyricsResult>,
}

impl LyricsWrite {
    fn new(outcome: LyricsWriteOutcome, result: Option<LyricsResult>) -> Self {
        Self { outcome, result }
    }
}

/// The lyrics in a song's own tags, and its length: what the C# read and wrote with TagLib. A
/// seam, so tests can hold a song's tags in memory.
pub trait LyricsTagAccess: Send + Sync {
    /// `file.Tag.Lyrics`, or an error when the file cannot be read as audio.
    fn read_lyrics(&self, path: &Path) -> io::Result<Option<String>>;

    /// `file.Tag.Lyrics = lyrics; file.Save()`. None clears them.
    fn write_lyrics(&self, path: &Path, lyrics: Option<&str>) -> anyhow::Result<()>;

    /// `file.Properties.Duration`, rounded to whole seconds; None when unknown or not positive.
    fn duration_seconds(&self, path: &Path) -> Option<i32>;

    /// What the library job reads of a song (`TagLib.File.Create(path)`): its tags and length.
    /// `Ok(None)` when the file cannot be read as audio (TagLib's own exceptions, which the job
    /// skips the song for); `Err` for an I/O error (`IOException`/`UnauthorizedAccessException`,
    /// which a scan counts as failed).
    fn read_song(&self, path: &Path) -> io::Result<Option<LyricsSongTags>>;
}

/// A song's tags as the library job reads them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsSongTags {
    /// `Tag.FirstPerformer`: the first performer, None when there is none.
    pub first_performer: Option<String>,
    /// `Tag.FirstAlbumArtist`.
    pub first_album_artist: Option<String>,
    /// `Tag.Title`.
    pub title: Option<String>,
    /// `Tag.Album`.
    pub album: Option<String>,
    /// `Tag.Lyrics`.
    pub lyrics: Option<String>,
    /// `Properties.Duration`, rounded (`Math.Round`) to whole seconds; None when not positive.
    pub duration_seconds: Option<i32>,
}

/// The song's tags, through the tags port (`octo_media::tags`, TagLib's stand-in).
pub struct TagLibLyricsTags;

impl LyricsTagAccess for TagLibLyricsTags {
    fn read_lyrics(&self, path: &Path) -> io::Result<Option<String>> {
        song_lyrics::read_tag_lyrics(path)
    }

    fn write_lyrics(&self, path: &Path, lyrics: Option<&str>) -> anyhow::Result<()> {
        octo_media::tags::write_lyrics(path, lyrics)?;
        Ok(())
    }

    fn duration_seconds(&self, path: &Path) -> Option<i32> {
        let seconds = octo_media::tags::TagFile::open(path).ok()?.duration_seconds();
        (seconds > 0).then_some(seconds)
    }

    fn read_song(&self, path: &Path) -> io::Result<Option<LyricsSongTags>> {
        match octo_media::tags::TagFile::open(path) {
            Ok(file) => {
                let seconds = file.duration_seconds();
                Ok(Some(LyricsSongTags {
                    first_performer: file.first_performer(),
                    first_album_artist: file.first_album_artist(),
                    title: file.title(),
                    album: file.album(),
                    lyrics: file.lyrics(),
                    duration_seconds: (seconds > 0).then_some(seconds),
                }))
            }
            // An I/O failure is counted as a failed song; anything TagLib would have thrown
            // its own exception for (not audio, a format it cannot read) is skipped.
            Err(octo_media::tags::TagError::Io(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }
}

/// What a Save tells the undo journal before each write: the path, its kind
/// ([`LyricsUndoJournal::BESIDE`] or [`LyricsUndoJournal::INSIDE`]), and what was there.
pub type UndoRecord<'a> = &'a (dyn Fn(&Path, &str, Option<&str>) -> anyhow::Result<()> + Send + Sync);

/// Writes a lyrics file beside a download (#52), off the download path. Downloads run one at a
/// time under a lock, and LRCLIB can take seconds or shed load with a 503, so fetching lyrics
/// inline would stall every download queued behind this one. A sidecar Navidrome reads at
/// request time needs no rescan, so arriving a little later costs nothing.
///
/// Synced lyrics go in a .lrc, plain ones in a .txt: both are in Navidrome's default
/// LyricsPriority, and a .txt says plainly that there is no timing. Word-timed lyrics are
/// enhanced LRC: every line keeps its standard [mm:ss.xx] tag, so any player that reads .lrc
/// shows them line by line, and one that knows <mm:ss.xx> word tags (Navidrome among them,
/// which turns them into OpenSubsonic word cues) gets the words too.
///
/// A .lrc Octo writes opens with [re:Octo], LRC's own "made by" tag, which every reader skips,
/// and so do lyrics Octo writes inside a song (LYRICS_SAVE_TO inside or both). It is how Octo
/// knows lyrics are its own: an instrumental gets nothing, and lyrics Octo did not write, beside
/// the song or inside it, are never replaced. Asked to upgrade, Octo looks again for a song
/// whose lyrics are weaker than the sources would choose now (the song's own lyrics rank like
/// any source, see LYRICS_SOURCES): its own are replaced, and anyone else's get the better ones
/// beside them, as a .lrc, which Navidrome serves ahead of lyrics in the tags.
pub struct LyricsSidecarWriter {
    lyrics: Arc<LyricsService>,
    settings: Arc<SettingsStore>,
    tags: Arc<dyn LyricsTagAccess>,
    /// Resolved when it is needed in C# (`IServiceScopeFactory`); set once the library service
    /// exists. Without it no scan is asked for, as in the C# tests.
    library: OnceLock<Arc<dyn ILocalLibraryService>>,
    sender: mpsc::Sender<LyricsJob>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<LyricsJob>>,
    scan_pending: Arc<AtomicBool>,
}

impl LyricsSidecarWriter {
    /// How long a job whose services did not answer waits before it is tried again.
    pub const RETRY_DELAY: Duration = Duration::from_secs(10 * 60);
    pub const MAX_ATTEMPTS: i32 = 3;

    /// The first line of every .lrc Octo writes.
    pub const OCTO_MARK: &'static str = OCTO_MARK;

    /// How long after lyrics are written inside a song Navidrome is asked to scan, so a run of
    /// writes asks once.
    pub const SCAN_DELAY: Duration = Duration::from_secs(60);

    /// The queue holds this many jobs; a job past that is dropped (`DropWrite`).
    const QUEUE_CAPACITY: usize = 256;

    pub fn new(lyrics: Arc<LyricsService>, settings: Arc<SettingsStore>) -> Self {
        Self::with_tags(lyrics, settings, Arc::new(TagLibLyricsTags))
    }

    pub fn with_tags(
        lyrics: Arc<LyricsService>,
        settings: Arc<SettingsStore>,
        tags: Arc<dyn LyricsTagAccess>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(Self::QUEUE_CAPACITY);
        Self {
            lyrics,
            settings,
            tags,
            library: OnceLock::new(),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            scan_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The library service, asked for a scan after lyrics are written inside songs. Only the
    /// first call counts.
    pub fn set_library(&self, library: Arc<dyn ILocalLibraryService>) {
        let _ = self.library.set(library);
    }

    fn save_to(&self) -> &'static str {
        LyricsSaveTo::normalize(Some(&self.settings.current().metadata.save_lyrics_to))
    }

    /// Queue a song. False when the queue is full and the job was dropped.
    pub fn try_enqueue(&self, job: LyricsJob) -> bool {
        self.sender.try_send(job).is_ok()
    }

    /// The hosted service's loop (`ExecuteAsync`): one job at a time until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut queue = self.receiver.lock().await;
        loop {
            let job = tokio::select! {
                job = queue.recv() => job,
                () = stopping.cancelled() => break,
            };
            let Some(job) = job else {
                break;
            };
            // One job's failure is logged and the loop goes on.
            match self.write(&job, false, &stopping).await {
                _ if stopping.is_cancelled() => break,
                Ok(write) if write.outcome == LyricsWriteOutcome::Retrying => {
                    self.retry_later(job.clone().with_attempt(job.attempt + 1), stopping.clone());
                }
                Ok(_) => {}
                Err(error) => warn!("Could not write lyrics for {}: {error}", job.audio_path.display()),
            }
        }
        Ok(())
    }

    /// Look the song up and save what was found. A song that already has lyrics is left alone,
    /// unless `upgrade`: then it is looked up with its own lyrics ranked among the sources, and
    /// better ones are saved (see the type's summary for where).
    pub async fn write(
        &self,
        job: &LyricsJob,
        upgrade: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<LyricsWrite> {
        if !job.audio_path.exists() {
            return Ok(LyricsWrite::new(LyricsWriteOutcome::Gone, None));
        }
        let has = self.song_lyrics(&job.audio_path);
        if has.place != SongLyricsPlace::None && (!upgrade || has.timing == LyricsTiming::Word || has.unknown)
        {
            return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
        }

        let lookup = self.lyrics.find(&self.query_of(job), ct, has.timing).await;
        if lookup.transient {
            if job.attempt < Self::MAX_ATTEMPTS {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::Retrying, None));
            }
            info!(
                "No lyrics service answered for '{} - {}' after {} tries",
                job.artist, job.title, job.attempt
            );
            return Ok(LyricsWrite::new(LyricsWriteOutcome::GaveUp, None));
        }

        if has.place != SongLyricsPlace::None {
            let Some(better) = lookup
                .result
                .filter(|better| !better.is_songs_own() && better.timing() > has.timing)
            else {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
            };
            if !self.save(&job.audio_path, &better, None).await? {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
            }
            info!(
                "Lyrics for '{} - {}' upgraded from {:?} to {:?} from {}",
                job.artist,
                job.title,
                has.timing,
                better.timing(),
                better.source
            );
            return Ok(LyricsWrite::new(LyricsWriteOutcome::Upgraded, Some(better)));
        }

        match lookup.result {
            Some(instrumental) if instrumental.instrumental => {
                info!(
                    "{} says '{} - {}' is instrumental; no lyrics file",
                    instrumental.source, job.artist, job.title
                );
                Ok(LyricsWrite::new(
                    LyricsWriteOutcome::Instrumental,
                    Some(instrumental),
                ))
            }
            Some(found) if !found.is_songs_own() && (found.has_synced() || found.has_plain()) => {
                if !self.save(&job.audio_path, &found, None).await? {
                    return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
                }
                info!(
                    "Lyrics for '{} - {}' from {} ({})",
                    job.artist,
                    job.title,
                    found.source,
                    to_lower_invariant(&format!("{:?}", found.timing()))
                );
                Ok(LyricsWrite::new(LyricsWriteOutcome::Written, Some(found)))
            }
            _ => {
                info!("No lyrics found for '{} - {}'", job.artist, job.title);
                Ok(LyricsWrite::new(LyricsWriteOutcome::NotFound, None))
            }
        }
    }

    /// What the sources find for a song, its own lyrics ranked among them, without saving
    /// anything: the lyrics page's preview.
    pub async fn look_up(
        &self,
        job: &LyricsJob,
        songs_own: LyricsTiming,
        ct: &CancellationToken,
    ) -> LyricsLookup {
        self.lyrics.find(&self.query_of(job), ct, songs_own).await
    }

    fn query_of(&self, job: &LyricsJob) -> LyricsQuery {
        LyricsQuery::new(
            job.artist.clone(),
            job.title.clone(),
            job.album.clone(),
            job.duration_seconds
                .or_else(|| self.tags.duration_seconds(&job.audio_path)),
        )
    }

    /// Replace a song's lyrics with lyrics someone chose. Only where Octo may write: where the
    /// song has no lyrics, or only Octo's; lyrics the owner put there are never touched.
    pub async fn replace(
        &self,
        audio_path: &Path,
        chosen: &LyricsResult,
        _ct: &CancellationToken,
    ) -> anyhow::Result<bool> {
        if !audio_path.exists() || (!chosen.has_synced() && !chosen.has_plain()) {
            return Ok(false);
        }
        let has = self.song_lyrics(audio_path);
        if has.place != SongLyricsPlace::None && !has.octos {
            return Ok(false);
        }
        self.save(audio_path, chosen, None).await
    }

    /// Save lyrics someone picked on the lyrics page, where LYRICS_SAVE_TO says and Octo may
    /// write, telling `record` what each write replaces (path, kind, what was there) before it
    /// happens. False when there was nowhere Octo may write.
    pub async fn save_chosen(
        &self,
        audio_path: &Path,
        found: &LyricsResult,
        record: UndoRecord<'_>,
        _ct: &CancellationToken,
    ) -> anyhow::Result<bool> {
        self.save(audio_path, found, Some(record)).await
    }

    /// Save lyrics where LYRICS_SAVE_TO says, replacing only Octo's own. Where that is not
    /// allowed (inside a song whose tags hold someone else's lyrics), they go beside it
    /// instead, which takes nothing away. False when there was nowhere to put them.
    async fn save(
        &self,
        audio_path: &Path,
        found: &LyricsResult,
        record: Option<UndoRecord<'_>>,
    ) -> anyhow::Result<bool> {
        let stem = song_lyrics::stem(audio_path);
        let save_to = self.save_to();
        let inside = save_to != LyricsSaveTo::BESIDE && self.may_write_inside(audio_path);
        let beside = save_to != LyricsSaveTo::INSIDE || !inside;
        let mut saved = false;
        if inside {
            if let Some(record) = record {
                record(
                    audio_path,
                    LyricsUndoJournal::INSIDE,
                    self.tag_lyrics(audio_path).as_deref(),
                )?;
            }
            if self.write_inside(audio_path, found) {
                saved = true;
                self.scan_soon();
            }
        }
        if beside && song_lyrics::may_write_beside(&stem, found.has_synced()) {
            let target = with_suffix(&stem, if found.has_synced() { ".lrc" } else { ".txt" });
            if let Some(record) = record {
                let before = if target.exists() {
                    Some(tokio::fs::read(&target).await.map(|bytes| utf8_text(&bytes))?)
                } else {
                    None
                };
                record(&target, LyricsUndoJournal::BESIDE, before.as_deref())?;
            }
            Self::write_file(&stem, found).await?;
            saved = true;
        }
        Ok(saved)
    }

    /// Put back what one Save replaced. A file Octo added is removed, and only while it is
    /// still Octo's; lyrics in a song's tags go back only while the tags hold Octo's. False when
    /// the file has changed since, and was left alone.
    pub fn restore(&self, entry: &LyricsUndoEntry) -> bool {
        let path = Path::new(&entry.path);
        let restored = (|| -> anyhow::Result<bool> {
            if entry.kind == LyricsUndoJournal::INSIDE {
                if !path.exists() || !self.may_write_inside(path) {
                    return Ok(false);
                }
                self.tags.write_lyrics(path, entry.before.as_deref())?;
                self.scan_soon();
                return Ok(true);
            }
            let ours = if ends_with_ignore_case(&entry.path, ".lrc") {
                Self::is_octos(path)
            } else {
                path.exists()
            };
            if !ours {
                return Ok(false);
            }
            match &entry.before {
                None => std::fs::remove_file(path)?,
                Some(before) => std::fs::write(path, before.as_bytes())?,
            }
            Ok(true)
        })();
        restored.unwrap_or_else(|error| {
            warn!("Could not put back the lyrics of {}: {error}", entry.path);
            false
        })
    }

    /// Whether a .lrc opens with Octo's own mark.
    pub fn is_octos(lrc_path: &Path) -> bool {
        song_lyrics::is_octos(lrc_path)
    }

    /// The tags seam the writer reads and writes songs through, which the library job reads
    /// songs through too.
    pub fn tags(&self) -> &Arc<dyn LyricsTagAccess> {
        &self.tags
    }

    /// `SongLyrics.Of`, with the tags read through the seam.
    pub fn song_lyrics(&self, audio_path: &Path) -> SongLyrics {
        let inside = self.tags.read_lyrics(audio_path).ok().flatten();
        song_lyrics::of_with_tag_lyrics(audio_path, inside.as_deref())
    }

    /// `SongLyrics.MayWriteInside`: the tags hold no lyrics, or Octo's.
    fn may_write_inside(&self, audio_path: &Path) -> bool {
        match self.tags.read_lyrics(audio_path) {
            Ok(Some(text)) if !is_null_or_white_space(Some(&text)) => SongLyrics::is_octos_text(&text),
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// `ReadTagLyrics`: the lyrics in the tags, None when there are none or they cannot be read.
    fn tag_lyrics(&self, audio_path: &Path) -> Option<String> {
        self.tags
            .read_lyrics(audio_path)
            .ok()
            .flatten()
            .filter(|text| !text.is_empty())
    }

    /// UTF-8 without a byte order mark; not written atomically, as the C# wrote it.
    async fn write_file(stem: &Path, found: &LyricsResult) -> io::Result<()> {
        if found.has_synced() {
            let text = found.synced.as_deref().unwrap_or("").replace("\r\n", "\n");
            tokio::fs::write(
                with_suffix(stem, ".lrc"),
                format!("{OCTO_MARK}\n{}\n", text.trim()),
            )
            .await
        } else {
            let text = found.plain.as_deref().unwrap_or("").replace("\r\n", "\n");
            tokio::fs::write(with_suffix(stem, ".txt"), format!("{}\n", text.trim())).await
        }
    }

    /// The lyrics in the song's own tags, marked as Octo's. False when the file could not be
    /// written.
    fn write_inside(&self, audio_path: &Path, found: &LyricsResult) -> bool {
        let text = if found.has_synced() {
            found.synced.as_deref()
        } else {
            found.plain.as_deref()
        }
        .unwrap_or("")
        .replace("\r\n", "\n");
        match self
            .tags
            .write_lyrics(audio_path, Some(&format!("{OCTO_MARK}\n{}", text.trim())))
        {
            Ok(()) => true,
            Err(error) => {
                warn!("Could not write lyrics inside {}: {error}", audio_path.display());
                false
            }
        }
    }

    /// Navidrome reads a song's tags only when it scans, so one scan is asked for a little
    /// after lyrics are written inside, once for a run of them.
    fn scan_soon(&self) {
        let Some(library) = self.library.get().cloned() else {
            return;
        };
        if self.scan_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending = self.scan_pending.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            pending.store(false, Ordering::SeqCst);
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(Self::SCAN_DELAY).await;
            pending.store(false, Ordering::SeqCst);
            // The C# caught and logged an exception here; the trait's answer is only whether
            // a scan started, which the C# ignored too.
            library.trigger_library_scan(false).await;
        });
    }

    fn retry_later(&self, job: LyricsJob, stopping: CancellationToken) {
        let sender = self.sender.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(Self::RETRY_DELAY) => {
                    let _ = sender.try_send(job);
                }
                // Shutting down.
                () = stopping.cancelled() => {}
            }
        });
    }
}

/// `s.EndsWith(suffix, StringComparison.OrdinalIgnoreCase)`.
fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    let count = suffix.chars().count();
    let Some((start, _)) = s.char_indices().rev().nth(count.saturating_sub(1)) else {
        return false;
    };
    count > 0 && eq_ignore_case(&s[start..], suffix)
}

fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut path: OsString = stem.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// `File.ReadAllText`: UTF-8, a byte order mark skipped, bytes that are not UTF-8 as U+FFFD.
fn utf8_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
#[path = "lyrics_sidecar_writer_tests.rs"]
mod tests;
