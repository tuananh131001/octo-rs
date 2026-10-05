//! Port of `Services/Library/DuplicateScanWorker.cs`: the walk for recordings the library holds
//! more than once (#53), handed to the Duplicates playlists.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{self, eq_ignore_case};
use octo_core::fingerprint::TrackMatchComparer;
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::settings::{LibraryActionSettings, SettingsStore};
use octo_media::audio::{SpectrumAnalyzer, SpectrumReport, SpectrumVerdict};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::NavidromeSongPathResolver;
use super::notice_queue::NoticeQueue;
use super::quality_upgrade_worker::compare_ordinal;
use crate::services::framework::http::{HttpAnswer, parse_json};
use crate::services::subsonic::NavidromeIdentityService;

/// One library row, as much of it as telling copies apart needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub recording_id: String,
    pub suffix: String,
    pub bit_rate: i32,
    pub duration: i32,

    /// For a lossless file whose spectrum says it was made from a lossy one, what it was likely
    /// made from ("about 128 kbps MP3"). Only ever set on a copy inside a duplicate group.
    pub transcoded_from: Option<String>,
}

impl LibraryTrack {
    /// The positional part of the C# record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        title: &str,
        artist: &str,
        album: &str,
        recording_id: &str,
        suffix: &str,
        bit_rate: i32,
        duration: i32,
    ) -> Self {
        LibraryTrack {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            recording_id: recording_id.to_string(),
            suffix: suffix.to_string(),
            bit_rate,
            duration,
            transcoded_from: None,
        }
    }
}

/// Copies of one recording, the one worth keeping first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub key: String,
    pub tracks: Vec<LibraryTrack>,
}

/// What the last walk found, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateScanResult {
    pub at_utc: DateTime<Utc>,
    pub tracks: usize,
    pub groups: usize,
    pub added: usize,
    pub complete: bool,
}

const LOSSLESS_SUFFIXES: [&str; 8] = ["flac", "alac", "wav", "aiff", "aif", "ape", "wv", "dsf"];

/// ALAC arrives as m4a, told apart from AAC only by its bitrate.
pub fn is_lossless_file(suffix: &str, bit_rate: i32) -> bool {
    LOSSLESS_SUFFIXES.iter().any(|s| eq_ignore_case(s, suffix))
        || (eq_ignore_case(suffix, "m4a") && bit_rate > 500)
}

pub fn is_lossless(track: &LibraryTrack) -> bool {
    is_lossless_file(&track.suffix, track.bit_rate)
}

/// Walks the library for recordings it holds more than once (#53) and hands them to the
/// Duplicates playlists. It only points them out: nothing here touches a file.
///
/// A duplicate is two files with the same MusicBrainz recording id AND the same version: a live
/// take, a remix, a radio edit or a second part shares a recording id surprisingly often, and
/// someone who keeps the album cut and the radio edit keeps both on purpose. Files without a
/// recording id are never grouped, because a guess about which files are the same song is a
/// guess someone would act on.
pub struct DuplicateScanWorker {
    queue: Arc<NoticeQueue>,
    identity: NavidromeIdentityService,
    http: reqwest::Client,
    /// `IOptionsMonitor` of the Subsonic, library action and Soulseek settings, read at use.
    settings: Arc<SettingsStore>,
    resolver: Option<Arc<NavidromeSongPathResolver>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
    /// `SemaphoreSlim(0, 1)`: one stored request, a second absorbed.
    requested: Notify,
    last_scan_utc: Mutex<DateTime<Utc>>,
    last_result: Mutex<Option<DuplicateScanResult>>,

    /// Spectrum verdicts by track id, size and modification time, so a file is decoded once and
    /// not again on every scan. A replaced or re-tagged file has a new key and is looked at again.
    spectra: Mutex<HashMap<String, SpectrumReport>>,
}

impl DuplicateScanWorker {
    pub const PAGE_SIZE: usize = 500;

    /// 100,000 tracks. A library larger than that is scanned in part, which can add questions
    /// but never settle one.
    pub const MAX_PAGES: usize = 200;

    const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
    const CHECK_INTERVAL: Duration = Duration::from_secs(60);

    /// How many files one scan may decode. A second or so each, and only copies inside a group
    /// with two or more lossless files are ever looked at; the rest wait for the next scan.
    pub const MAX_SPECTRUM_CHECKS_PER_SCAN: usize = 200;

    pub fn new(
        queue: Arc<NoticeQueue>,
        identity: NavidromeIdentityService,
        http: reqwest::Client,
        settings: Arc<SettingsStore>,
        resolver: Option<Arc<NavidromeSongPathResolver>>,
        spectrum: Option<Arc<SpectrumAnalyzer>>,
    ) -> Self {
        DuplicateScanWorker {
            queue,
            identity,
            http,
            settings,
            resolver,
            spectrum,
            requested: Notify::new(),
            last_scan_utc: Mutex::new(datetime::min_value()),
            last_result: Mutex::new(None),
            spectra: Mutex::new(HashMap::new()),
        }
    }

    pub fn last_result(&self) -> Option<DuplicateScanResult> {
        self.last_result.lock().clone()
    }

    /// The dashboard's "Scan now". A scan already waiting to run absorbs a second request.
    pub fn request_scan(&self) {
        self.requested.notify_one();
    }

    /// `ExecuteAsync`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut wait = Self::FIRST_CHECK;
        while !stopping.is_cancelled() {
            let requested = tokio::select! {
                answer = tokio::time::timeout(wait, self.requested.notified()) => answer.is_ok(),
                _ = stopping.cancelled() => break,
            };
            wait = Self::CHECK_INTERVAL;

            // Read afresh every tick, so switching Duplicates on needs no restart and the first
            // scan comes a minute later rather than a day later.
            let settings = self.settings.current().library_actions.clone();
            if !settings.enabled || !settings.duplicates_enabled || !self.identity.has_admin_identity() {
                continue;
            }
            let interval = chrono::TimeDelta::from_std(settings.effective_duplicates_scan_interval())
                .unwrap_or(chrono::TimeDelta::MAX);
            if !requested && Utc::now() - *self.last_scan_utc.lock() < interval {
                continue;
            }

            // Per-scan catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so one unhandled exception here would take Octo down.
            let scan = tokio::select! {
                scan = self.scan(&settings) => scan,
                _ = stopping.cancelled() => break,
            };
            if let Err(e) = scan {
                error!("Duplicate scan failed: {e}");
            }
            *self.last_scan_utc.lock() = Utc::now();
        }
        Ok(())
    }

    async fn scan(&self, settings: &LibraryActionSettings) -> anyhow::Result<()> {
        let (tracks, complete) = self.walk().await?;
        let mut groups = Self::find_groups(&tracks);
        let detect = self.settings.current().soulseek.detect_transcodes;
        if self.resolver.is_some() && self.spectrum.is_some() && detect {
            let decoded = AtomicUsize::new(0);
            groups = Self::check_transcodes(groups, |track| {
                let track = track.clone();
                let decoded = &decoded;
                async move {
                    if decoded.load(Ordering::SeqCst) >= Self::MAX_SPECTRUM_CHECKS_PER_SCAN {
                        None
                    } else {
                        self.spectrum_of(&track, decoded).await
                    }
                }
            })
            .await;
        }
        let users: Vec<&str> = settings
            .allowed_users
            .iter()
            .map(String::as_str)
            .filter(|user| !dotnet::is_blank(user))
            .collect();
        let added = self.queue.sync_duplicates(&groups, &users, complete);
        self.queue.flush();

        *self.last_result.lock() = Some(DuplicateScanResult {
            at_utc: Utc::now(),
            tracks: tracks.len(),
            groups: groups.len(),
            added,
            complete,
        });
        info!(
            "Duplicate scan: {} tracks with a recording id, {} groups, {added} new question(s){}",
            tracks.len(),
            groups.len(),
            if complete {
                ""
            } else {
                " (the walk did not finish, so nothing was settled)"
            }
        );
        Ok(())
    }

    /// Every track with a recording id, by paging search3 with the empty query, the same walk
    /// Symfonium makes to copy a library. Complete only when a short page ends it. `Err` where
    /// the C# threw: the request failed, or the answer was not JSON of the expected shape.
    pub async fn walk(&self) -> anyhow::Result<(Vec<LibraryTrack>, bool)> {
        let base_url = self
            .settings
            .current()
            .subsonic
            .url
            .clone()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        // The scan credential comes from the admin login, so make sure there has been one.
        self.identity.ensure_admin_jwt().await;
        let auth = self.identity.get_scan_auth();
        let (Some((user, token, salt)), false) = (auth, base_url.is_empty()) else {
            return Ok((Vec::new(), false));
        };

        let mut tracks = Vec::new();
        for page in 0..Self::MAX_PAGES {
            let url = format!(
                "{base_url}/rest/search3?f=json&c=octo&v=1.16.1&query=%22%22&songCount={}&songOffset={}&albumCount=0&artistCount=0&u={}&t={}&s={}",
                Self::PAGE_SIZE,
                page * Self::PAGE_SIZE,
                dotnet::escape_data_string(&user),
                dotnet::escape_data_string(&token),
                dotnet::escape_data_string(&salt)
            );
            let answer = HttpAnswer::read(self.http.get(&url).send().await?).await?;
            if !answer.is_success() {
                return Ok((tracks, false));
            }
            let root = parse_json(&answer.body)?;
            let Some(rows) = Self::parse_page(&root, &mut tracks).map_err(anyhow::Error::msg)? else {
                return Ok((tracks, false));
            };
            if rows < Self::PAGE_SIZE {
                return Ok((tracks, true));
            }
        }

        warn!(
            "The duplicate scan stopped at {} tracks. Pairs past that point are not found, and nothing is settled from a partial walk.",
            Self::MAX_PAGES * Self::PAGE_SIZE
        );
        Ok((tracks, false))
    }

    /// Adds the page's tracks that carry a recording id, and returns how many rows the page held,
    /// or None when it is not a successful answer at all (a refusal must not read as the end).
    /// `Err` where the C# threw: a member read on something that is not an object, or a status
    /// that is not a string.
    pub fn parse_page(root: &Value, into: &mut Vec<LibraryTrack>) -> Result<Option<usize>, String> {
        let Some(envelope) = property(root, "subsonic-response")? else {
            return Ok(None);
        };
        let ok = match property(envelope, "status")? {
            None => false,
            Some(Value::String(status)) => status == "ok",
            Some(Value::Null) => false,
            Some(_) => {
                return Err("The requested operation requires an element of type 'String'.".to_string());
            }
        };
        if !ok {
            return Ok(None);
        }
        let Some(result) = property(envelope, "searchResult3")? else {
            return Ok(Some(0));
        };
        let Some(Value::Array(songs)) = property(result, "song")? else {
            return Ok(Some(0));
        };

        let mut rows = 0;
        for song in songs {
            rows += 1;
            let id = text(song, "id")?;
            let recording = text(song, "musicBrainzId")?;
            let (Some(id), Some(recording)) =
                (id.filter(|s| !s.is_empty()), recording.filter(|s| !s.is_empty()))
            else {
                continue;
            };
            into.push(LibraryTrack {
                id,
                title: text(song, "title")?.unwrap_or_default(),
                artist: text(song, "artist")?.unwrap_or_default(),
                album: text(song, "album")?.unwrap_or_default(),
                recording_id: recording,
                suffix: text(song, "suffix")?.unwrap_or_default(),
                bit_rate: int(song, "bitRate")?,
                duration: int(song, "duration")?,
                transcoded_from: None,
            });
        }
        Ok(Some(rows))
    }

    /// Copies of one recording in one version. Grouped by recording id first, then clustered by
    /// title and artist, because a recording id alone groups a remix or a live take with the
    /// original more often than it should.
    pub fn find_groups(tracks: &[LibraryTrack]) -> Vec<DuplicateGroup> {
        // GroupBy(RecordingId, OrdinalIgnoreCase): groups in the order their first track came.
        let mut recordings: Vec<(String, Vec<&LibraryTrack>)> = Vec::new();
        for track in tracks.iter().filter(|track| !track.recording_id.is_empty()) {
            match recordings
                .iter_mut()
                .find(|(key, _)| eq_ignore_case(key, &track.recording_id))
            {
                Some((_, members)) => members.push(track),
                None => recordings.push((track.recording_id.clone(), vec![track])),
            }
        }

        let strict = SongIdentity::strict_titles();
        let mut groups = Vec::new();
        for (_, mut recording) in recordings {
            recording.sort_by(|a, b| compare_ordinal(&a.id, &b.id));
            let mut clusters: Vec<Vec<LibraryTrack>> = Vec::new();
            for track in recording {
                let home = clusters.iter_mut().find(|cluster| {
                    let first = &cluster[0];
                    SongIdentity::same_title(&track.title, &first.title, Some(&strict)).is_same()
                        && TrackMatchComparer::artist_matches(
                            &track.artist,
                            &first.artist,
                            &[first.artist.as_str()],
                        )
                        && TrackMatchComparer::artist_matches(
                            &first.artist,
                            &track.artist,
                            &[track.artist.as_str()],
                        )
                });
                match home {
                    Some(cluster) => cluster.push(track.clone()),
                    None => clusters.push(vec![track.clone()]),
                }
            }

            for cluster in clusters.into_iter().filter(|cluster| cluster.len() > 1) {
                let mut ids: Vec<&str> = cluster.iter().map(|track| track.id.as_str()).collect();
                ids.sort_by(|a, b| compare_ordinal(a, b));
                groups.push(DuplicateGroup {
                    key: format!("dup|{}", ids.join(",")),
                    tracks: Self::rank_for_keeping(&cluster),
                });
            }
        }
        groups
    }

    /// The groups again, with every lossless copy in a group that has two or more of them
    /// checked for being a transcode, and each group ranked again. Only those copies: a lone
    /// lossless file outranks the lossy ones either way, and the rest of the library is never
    /// decoded. A copy the check could not judge counts as genuine.
    pub async fn check_transcodes<F, Fut>(groups: Vec<DuplicateGroup>, mut check: F) -> Vec<DuplicateGroup>
    where
        F: FnMut(&LibraryTrack) -> Fut,
        Fut: Future<Output = Option<SpectrumReport>>,
    {
        let mut result = Vec::with_capacity(groups.len());
        for group in groups {
            if group.tracks.iter().filter(|track| is_lossless(track)).count() < 2 {
                result.push(group);
                continue;
            }
            let mut tracks = Vec::with_capacity(group.tracks.len());
            for track in &group.tracks {
                let report = if is_lossless(track) {
                    check(track).await
                } else {
                    None
                };
                match report {
                    Some(report) if report.is_likely_lossy() => tracks.push(LibraryTrack {
                        transcoded_from: report.estimate.clone(),
                        ..track.clone()
                    }),
                    _ => tracks.push(track.clone()),
                }
            }
            result.push(DuplicateGroup {
                key: group.key,
                tracks: Self::rank_for_keeping(&tracks),
            });
        }
        result
    }

    async fn spectrum_of(&self, track: &LibraryTrack, decoded: &AtomicUsize) -> Option<SpectrumReport> {
        let (resolver, spectrum) = (self.resolver.as_ref()?, self.spectrum.as_ref()?);
        let file = resolver.resolve(&track.id).await?;
        let key = format!(
            "{}|{}|{}",
            track.id,
            file.size_bytes,
            last_write_ticks(&file.absolute_path)
        );
        if let Some(known) = self.spectra.lock().get(&key) {
            return Some(known.clone());
        }

        decoded.fetch_add(1, Ordering::SeqCst);
        let timeout = self
            .settings
            .current()
            .soulseek
            .effective_transcode_check_timeout_seconds();
        let report = spectrum.analyze(Path::new(&file.absolute_path), timeout).await;
        // Unknown from a timeout or a missing ffmpeg is not remembered, so it is asked again.
        if report.verdict != SpectrumVerdict::Unknown {
            self.spectra.lock().insert(key, report.clone());
        }
        if report.is_likely_lossy() {
            info!("Duplicate scan: {} is {}", file.absolute_path, report.describe());
        }
        Some(report)
    }

    /// Genuine lossless first, then a lossless file made from a lossy one, then the higher
    /// bitrate, then the length closest to the group's median (a copy much longer or shorter
    /// than the rest is the likelier to be cut or padded), then id.
    pub fn rank_for_keeping(group: &[LibraryTrack]) -> Vec<LibraryTrack> {
        let mut lengths: Vec<i32> = group.iter().map(|track| track.duration).collect();
        lengths.sort_unstable();
        let median = lengths[lengths.len() / 2];
        let mut ranked = group.to_vec();
        ranked.sort_by(|a, b| {
            let genuine = |t: &LibraryTrack| is_lossless(t) && t.transcoded_from.is_none();
            genuine(b)
                .cmp(&genuine(a))
                .then_with(|| is_lossless(b).cmp(&is_lossless(a)))
                .then_with(|| b.bit_rate.cmp(&a.bit_rate))
                .then_with(|| {
                    (i64::from(a.duration) - i64::from(median))
                        .abs()
                        .cmp(&(i64::from(b.duration) - i64::from(median)).abs())
                })
                .then_with(|| compare_ordinal(&a.id, &b.id))
        });
        ranked
    }
}

/// `JsonElement.TryGetProperty`, which throws on anything but an object.
fn property<'a>(element: &'a Value, name: &str) -> Result<Option<&'a Value>, String> {
    match element {
        Value::Object(map) => Ok(map.get(name)),
        _ => Err("The requested operation requires an element of type 'Object'.".to_string()),
    }
}

fn text(element: &Value, name: &str) -> Result<Option<String>, String> {
    Ok(match property(element, name)? {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    })
}

fn int(element: &Value, name: &str) -> Result<i32, String> {
    Ok(match property(element, name)? {
        Some(Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        _ => 0,
    })
}

/// `File.GetLastWriteTimeUtc(path).Ticks`: 1601-01-01 for a file that is not there.
fn last_write_ticks(path: &str) -> i64 {
    const MISSING: i64 = 504_911_232_000_000_000;
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| dotnet_ticks(DateTime::<Utc>::from(modified)))
        .unwrap_or(MISSING)
}

#[cfg(test)]
#[path = "duplicate_tests.rs"]
mod tests;
