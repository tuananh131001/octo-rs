//! What a download is and what is written to it: identification, the old catalog fill, the
//! album folder's facts, the loudness, the genre and the cover, the tags themselves, and the
//! album gain at the end of a walk. Part of `BaseDownloadService.cs`; the tag body itself is
//! `octo_media::tags::write_song`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::{SongIdentity, dotnet};
use octo_core::last_fm::last_fm_radio_recommendation_service::canonical_tag;
use octo_core::metadata::deezer_metadata_service::FullTrackMeta;
use octo_core::metadata::genre_normalizer::{GenreNormalizer, GenreTagAction};
use octo_core::models::domain::Song;
use octo_core::settings::{FolderStructure, GenreFallbackSource, GenreSettings, MetadataSettings};
use octo_core::tagging::{
    AlbumTagContext, CatalogBlankFiller, MeasuredLoudness, PreviewLoudnessMeter, ReleaseIdentifier,
    ReplayGainText,
};
use octo_media::audio::{ILoudnessMeter, Loudness, ReplayGainTags};
use octo_media::cover::cover_image;
use octo_media::tags::{
    GenreWrite, TagFile, embed_cover, front_cover, tag_writer_extras, write_album_gain, write_song,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::placement::{get_directory_name, get_file_name, is_audio_file};
use super::{BaseDownloadService, RequestedIdentity, get_extension};
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;

impl BaseDownloadService {
    /// Whether a landed file came from the video site's staging folder, whose tags are an
    /// uploader's and not evidence of anything.
    pub fn is_staged_upload(path: &str) -> bool {
        path.split(['/', '\\'])
            .any(|segment| dotnet::eq_ignore_case(segment, INCOMING_FOLDER_NAME))
    }

    /// Works out what the file is and sets the Song from it. Every candidate release the
    /// fingerprint service, the music database and the catalog offer is weighed against what was
    /// asked for and what landed; a sure match sets the album-level tags, a doubtful one only
    /// fills blanks, the way the catalog always did. Reads only: the tags are written by
    /// `write_metadata` once the file has been placed. Best-effort; a miss never breaks the
    /// download.
    pub async fn identify(
        &self,
        song: &mut Song,
        requested: &RequestedIdentity,
        file_path: &str,
        album: Option<&AlbumTagContext<Loudness>>,
    ) {
        // Last.fm/YouTube titles often carry a redundant "Artist - " prefix (e.g. "Radiohead - No
        // Surprises") which mislabels the file, so it goes from the written title. The lookup
        // gets the title whole: the catalog tries it without "(Official Video)" and guests on its
        // own, and a "(Live)" left in is what stops a live download being tagged with the studio
        // album's cover, track number and year.
        song.title = strip_artist_prefix(&song.artist, &song.title);
        let query_title = song.title.clone();
        let settings = self.settings.current().metadata.clone();

        let request = ReleaseIdentifier::request_for(
            song,
            &song.artist,
            &query_title,
            Some(&requested.album),
            requested.track,
        );
        let identified = self
            .identifier()
            .identify(
                song,
                request,
                file_path,
                !Self::is_staged_upload(file_path),
                album,
                &CancellationToken::new(),
            )
            .await;
        let mut planned = false;
        match identified {
            Ok(mut plan) => {
                planned = true;
                if settings.tag_rehearsal {
                    plan.apply_rehearsal_to(song);
                } else {
                    plan.apply_to(song);
                    if plan.album_from_candidate()
                        && let Some(chosen) = plan.fields.get("album").and_then(|f| f.value.as_deref())
                        && let Some(file_album) = plan
                            .evidence
                            .as_ref()
                            .and_then(|e| e.file.album.as_deref())
                            .filter(|a| !a.is_empty())
                        && SongIdentity::key(file_album) != SongIdentity::key(chosen)
                    {
                        info!(
                            "'{chosen}' replaces the file's own '{file_album}' ({})",
                            plan.confidence
                        );
                    }
                }
                let catalog_best = plan.catalog_best.clone();
                song.tag_plan = Some(Box::new(plan));
                Self::fill_blanks_from_catalog(song, catalog_best.as_ref());
            }
            Err(e) => {
                warn!(
                    "Identification failed for '{} - {}'; filling blanks the old way: {e}",
                    song.artist, song.title
                );
                self.fill_blanks_from_catalog_async(song, &query_title).await;
            }
        }

        fill_album_from_file(song, file_path);
        Self::apply_single_fallback(song, settings.album_from_title);

        if planned && !settings.tag_rehearsal {
            match album {
                Some(album) => {
                    album.pin(song);
                    if let Some(plan) = song.tag_plan.as_deref() {
                        album.capture(plan, song);
                    }
                }
                None if self.settings.current().subsonic.folder_structure == FolderStructure::Organized => {
                    self.pin_to_sibling(song, requested, file_path)
                }
                None => {}
            }
        }
    }

    /// The old catalog enrichment, asked for on its own when identification itself failed.
    async fn fill_blanks_from_catalog_async(&self, song: &mut Song, query_title: &str) {
        if let Some(deezer) = &self.services.deezer {
            let meta = deezer.enrich_track_full(&song.artist, query_title).await;
            Self::fill_blanks_from_catalog(song, meta.as_ref());
        }
    }

    /// Fills any missing metadata on `song` from the catalog's best hit. Existing values win (a
    /// well-tagged Soulseek FLAC is enriched, not overwritten); the catalog fills the gaps and
    /// supplies the cover. The album's own facts (its cover, its compilation flag) are taken only
    /// when the hit is the album the song is filed under, since the chooser may have put the song
    /// on another release than the catalog's first hit.
    pub fn fill_blanks_from_catalog(song: &mut Song, m: Option<&FullTrackMeta>) {
        let Some(m) = m else {
            return;
        };
        let empty = |value: &Option<String>| value.as_deref().is_none_or(str::is_empty);
        let same_album = song.album.is_empty()
            || empty(&m.album_title)
            || SongIdentity::key(&song.album) == SongIdentity::key(m.album_title.as_deref().unwrap_or(""));

        // The catalog's main artist names the folder when the request carried a list of credits
        // (#49); its contributors give every credited artist a value of their own, so Navidrome
        // files a collaboration under each of them.
        if empty(&song.primary_artist) && !empty(&m.artist_name) {
            song.primary_artist = m.artist_name.clone();
        }
        if song.artists.is_empty()
            && let Some(contributors) = m.contributors.as_ref().filter(|c| c.len() > 1)
        {
            song.artists = contributors.clone();
        }
        if song.album.is_empty()
            && let Some(title) = m.album_title.as_deref().filter(|t| !t.is_empty())
        {
            song.album = title.to_string();
        }
        if same_album {
            // The album's own artist, not the track's: the two differ on every feature and every
            // compilation.
            if empty(&song.album_artist)
                && let Some(album_artist) = m
                    .album_artist_name
                    .as_deref()
                    .or(m.artist_name.as_deref())
                    .filter(|a| !a.is_empty())
            {
                song.album_artist = Some(album_artist.to_string());
            }
            if Self::is_various_artists(m.album_artist_name.as_deref())
                || m.record_type
                    .as_deref()
                    .is_some_and(|t| dotnet::eq_ignore_case(t, "compile"))
            {
                song.is_compilation = true;
            }
            if empty(&song.cover_art_url_large) {
                song.cover_art_url_large = m.album_cover_url.clone();
            }
            if song.year.is_none() {
                song.year = m.year;
            }
            if song.track.is_none() {
                song.track = m.track_number;
            }
            if song.disc_number.is_none() {
                song.disc_number = m.disc_number;
            }
            if song.total_tracks.is_none() {
                song.total_tracks = m.total_tracks;
            }
            if empty(&song.label) {
                song.label = m.label.clone();
            }
            if empty(&song.barcode) {
                song.barcode = m.barcode.clone();
            }
            if empty(&song.release_date) {
                song.release_date = m.release_date.clone();
            }
        }
        if song.duration.is_none() {
            song.duration = m.duration;
        }
        if empty(&song.genre) {
            song.genre = m.genre.clone();
        }
        if empty(&song.isrc) {
            song.isrc = m.isrc.clone();
        }
    }

    /// A single joining an album folder that is already there takes that album's release facts
    /// from one of its files, when the two agree on the album and its artist, so the album the
    /// library server shows keeps one label, one catalogue number and one year.
    fn pin_to_sibling(&self, song: &mut Song, requested: &RequestedIdentity, current_path: &str) {
        if dotnet::is_blank(&song.album) || self.download_path().is_empty() {
            return;
        }
        let target = self.organized_target(song, requested, get_extension(current_path));
        let Some(dir) = get_directory_name(&target).filter(|d| !d.is_empty() && Path::new(d).is_dir()) else {
            return;
        };
        let current = get_full_path(current_path);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                debug!("could not read the album folder's sibling for {current_path}: {e}");
                return;
            }
        };
        let sibling = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path().to_string_lossy().into_owned())
            .find(|file| is_audio_file(file) && !dotnet::eq_ignore_case(&get_full_path(file), &current));
        let Some(sibling) = sibling else {
            return;
        };

        let facts = tag_writer_extras::read_facts(&sibling, true);
        if SongIdentity::key(facts.album.as_deref().unwrap_or("")) != SongIdentity::key(&song.album) {
            return;
        }
        let album_artist = song
            .album_artist
            .clone()
            .or_else(|| song.primary_artist.clone())
            .unwrap_or_else(|| song.artist.clone());
        if let Some(theirs) = facts.album_artist.as_deref().filter(|a| !a.is_empty())
            && !album_artist.is_empty()
            && !SongIdentity::same_artist_name(theirs, &album_artist)
        {
            return;
        }

        if let Some(year) = facts.year.filter(|y| *y > 0) {
            song.year = Some(year);
        }
        if let Some(label) = facts.label.filter(|l| !l.is_empty()) {
            song.label = Some(label);
        }
        if let Some(number) = facts.catalog_number.filter(|n| !n.is_empty()) {
            song.catalog_number = Some(number);
        }
        if let Some(barcode) = facts.barcode.filter(|b| !b.is_empty()) {
            song.barcode = Some(barcode);
        }
        if let Some(plan) = song.tag_plan.as_deref_mut() {
            plan.notes.push(format!(
                "album facts taken from the album folder's own '{}'",
                get_file_name(&sibling)
            ));
        }
    }

    /// Start measuring a landed file, beside identification, when ReplayGain is on.
    pub(super) fn start_loudness(&self, path: &str) -> Option<JoinHandle<Option<Loudness>>> {
        let settings = self.settings.current().metadata.clone();
        if !settings.replay_gain {
            return None;
        }
        let meter = Arc::clone(self.services.loudness_meter.as_ref()?);
        let (path, timeout) = (path.to_string(), settings.effective_replay_gain_timeout_seconds());
        Some(tokio::spawn(async move {
            meter
                .measure(Path::new(&path), timeout, &CancellationToken::new())
                .await
        }))
    }

    /// Wait for the measurement, which has its own cap, and set the song's ReplayGain.
    pub(super) async fn apply_loudness(
        &self,
        song: &mut Song,
        measurement: Option<JoinHandle<Option<Loudness>>>,
        path: &str,
    ) {
        let Some(measurement) = measurement else {
            return;
        };
        let clock = std::time::Instant::now();
        let loudness = match measurement.await {
            Ok(loudness) => loudness,
            Err(e) => {
                debug!("loudness measurement failed for {path}: {e}");
                None
            }
        };
        if let Some(plan) = song.tag_plan.as_deref_mut() {
            plan.stage_seconds
                .insert("loudness".to_string(), clock.elapsed().as_secs_f64());
            plan.integrated_lufs = loudness.map(|l| l.integrated_lufs);
            plan.true_peak_dbfs = loudness.map(|l| l.true_peak_dbfs);
        }
        let Some(tags) = ReplayGainTags::for_track(loudness.as_ref()) else {
            if let Some(plan) = song.tag_plan.as_deref_mut() {
                plan.notes
                    .push("the loudness could not be measured, so the file has no ReplayGain".to_string());
            }
            return;
        };
        song.replay_gain_track_gain_db = Some(tags.gain_db);
        song.replay_gain_track_peak = Some(tags.peak);
    }

    /// A track that still has no album is filed as a single under its own title (#50). That is a
    /// real, correct release, and it gives Navidrome something to group; the alternative is one
    /// "[Unknown Album]" collecting every unrelated album-less track. Skipped for a compilation,
    /// where a hundred one-track albums would be worse than the one untidy bucket.
    pub fn apply_single_fallback(song: &mut Song, enabled: bool) {
        if !enabled || !dotnet::is_blank(&song.album) || dotnet::is_blank(&song.title) {
            return;
        }
        if song.is_compilation || Self::is_various_artists(song.album_artist.as_deref()) {
            return;
        }
        song.album = song.title.trim().to_string();
        if song.album_artist.as_deref().is_none_or(str::is_empty) {
            song.album_artist = Some(song.primary_artist.clone().unwrap_or_else(|| song.artist.clone()));
        }
    }

    pub fn is_various_artists(name: Option<&str>) -> bool {
        name.map(str::trim).is_some_and(|value| {
            !value.is_empty()
                && ["Various Artists", "Various", "VA"]
                    .iter()
                    .any(|various| dotnet::eq_ignore_case(value, various))
        })
    }

    /// Last resort for a file with no usable genre: the listener-supplied tags Last.fm already
    /// caches for radio.
    ///
    /// Gated on `has_api_key`, NOT on radio being on. A user who turned radio off still
    /// configured a key, and tagging is not radio. Last.fm tags are also the rawest input the
    /// normaliser ever sees, so they go through radio's own vocabulary first: that list filters
    /// Last.fm data, which is exactly what it was written for, without the genre settings having
    /// to own it.
    async fn resolve_genre_fallback(&self, song: &Song, settings: &GenreSettings) -> Vec<String> {
        if settings.fallback == GenreFallbackSource::MusicBrainz {
            // The release lookup already carries the genres people voted on for the chosen
            // release. Two votes or more count, so one person's tag cannot name a genre. With no
            // votes it behaves as Last.fm and says so, because a setting that appears to work and
            // silently does nothing is worse than one that is missing.
            let voted = song
                .tag_plan
                .as_deref()
                .and_then(|plan| plan.details())
                .map(|details| details.top_genres(2, 3))
                .unwrap_or_default();
            if !voted.is_empty() {
                return voted;
            }
            info!(
                "MusicBrainz lists no voted genres for {} - {}; using Last.fm top tags",
                song.artist, song.title
            );
        }

        let Some(last_fm) = self.services.last_fm.as_ref().filter(|l| l.has_api_key()) else {
            return Vec::new();
        };
        let tags = async {
            let mut tags = last_fm.get_track_top_tags(&song.artist, &song.title, 8).await?;
            if tags.is_empty() && !song.artist.is_empty() {
                tags = last_fm.get_artist_top_tags(&song.artist, 8).await?;
            }
            anyhow::Ok(tags)
        }
        .await;
        match tags {
            Ok(tags) => tags
                .iter()
                .map(|tag| canonical_tag(tag))
                .filter(|tag| !tag.is_empty())
                .collect(),
            Err(e) => {
                debug!(
                    "genre fallback lookup failed for {} - {}: {e}",
                    song.artist, song.title
                );
                Vec::new()
            }
        }
    }

    /// Write the Song's tags and the chosen cover onto the file. Returns the cover it settled
    /// on, embedded or already there, so the same picture can go beside the file as cover.jpg;
    /// `None` when there was none or the write failed.
    ///
    /// The C# held the file open across the genre and cover lookups; here what they need is read
    /// first, and the file opened again to write, so no file is held across a wait.
    pub async fn write_metadata(&self, file_path: &str, song: &mut Song) -> Option<Vec<u8>> {
        info!("Writing metadata to: {file_path}");
        let (existing, embedded) = match TagFile::open(file_path) {
            Ok(file) => (file.genres(), front_cover(&file).map(|picture| picture.data)),
            Err(e) => {
                error!("Failed to write metadata to: {file_path}: {e}");
                return None;
            }
        };

        // Genre is the one tag that must be able to write NOTHING.
        //
        // Before this, genre was written ONLY when the song's genre was non-empty, and was never
        // cleared. So when the catalog missed and the source had no genre, whatever multi-value
        // frame the Soulseek peer's file or the yt-dlp output already carried survived untouched:
        // "People & Blogs" and seven-genres-at-once reached the library through the ABSENCE of a
        // write, not a bad one. The fix has to read the existing frame, normalise it, and write
        // the result back.
        let genre_settings = self.settings.current().genre.clone();
        let genres = if genre_settings.enabled {
            let mut plan = GenreNormalizer::plan(&existing, song.genre.as_deref(), &genre_settings, None);

            // Only pay for the lookup when nothing usable survived. Last.fm's top tags are weaker
            // evidence than a real genre frame and earn a turn only when there is no frame left.
            if plan.action != GenreTagAction::Write && genre_settings.fallback != GenreFallbackSource::None {
                let fallback = self.resolve_genre_fallback(song, &genre_settings).await;
                if !fallback.is_empty() {
                    plan = GenreNormalizer::plan(
                        &existing,
                        song.genre.as_deref(),
                        &genre_settings,
                        Some(&fallback),
                    );
                }
            }

            let write = match plan.action {
                GenreTagAction::Write => {
                    song.genre = plan.primary.clone();
                    GenreWrite::Write(plan.genres.clone())
                }
                GenreTagAction::Clear => {
                    // Destructive and not undoable per file, so it is logged below at
                    // Information: a user who regrets their mapping table can at least see what
                    // left.
                    song.genre = None;
                    GenreWrite::Clear
                }
                GenreTagAction::None => GenreWrite::Keep,
            };
            if plan.action != GenreTagAction::None {
                info!(
                    "Genre normalised for {file_path}: [{}] -> [{}]{}",
                    existing.join(", "),
                    plan.genres.join(", "),
                    plan.matched_rule
                        .as_deref()
                        .map(|rule| format!(" via {rule}"))
                        .unwrap_or_default()
                );
            }
            write
        } else if let Some(genre) = song.genre.as_deref().filter(|g| !g.is_empty()) {
            // Feature off: byte-for-byte the behaviour that shipped before this.
            GenreWrite::Write(vec![genre.to_string()])
        } else {
            GenreWrite::Keep
        };

        // One chain (#51) instead of one Deezer URL: Apple's master of the album, the Cover Art
        // Archive when a fingerprint named the release, the catalog's own cover, then Deezer,
        // iTunes and Last.fm by name, then the file's own art; the largest wins. A cover that is
        // not square counts as missing, and a letterboxed video frame gives up its centre. The
        // file gets it at 1500 px unless full size is asked for; cover.jpg gets it whole.
        let mut chosen_cover = None;
        let mut embed = None;
        if let Some(resolver) = &self.services.cover_resolver
            && let Some(cover) = resolver
                .resolve(song, embedded.as_deref(), &CancellationToken::new())
                .await
        {
            if !cover.keeps_existing {
                let bytes = if self.settings.current().metadata.embed_full_size_covers {
                    cover.bytes.clone()
                } else {
                    cover_image::fit_within(&cover.bytes, MetadataSettings::EMBEDDED_COVER_SIDE as u32)
                };
                info!("Cover art embedded from {}: {} bytes", cover.source, bytes.len());
                embed = Some(bytes);
            }
            chosen_cover = Some(cover.bytes);
        }

        let written = (|| {
            let mut file = TagFile::open(file_path)?;
            write_song(&mut file, song, &genres);
            if let Some(bytes) = embed {
                let mime = cover_image::mime_type(&bytes);
                embed_cover(&mut file, bytes, mime);
            }
            file.save()
        })();
        match written {
            Ok(()) => {
                info!("Metadata written successfully to: {file_path}");
                chosen_cover
            }
            Err(e) => {
                error!("Failed to write metadata to: {file_path}: {e}");
                None
            }
        }
    }

    /// The album gain and peak, written into every file the walk measured, once the walk ends.
    /// Only when every track was measured: an album gain for half an album is worse than none.
    /// A rewrite in place, so the library server keeps each file's id.
    pub fn write_album_gain(&self, context: &AlbumTagContext<Loudness>, album_title: &str) {
        let measured = context.loudness();
        if measured.is_empty() {
            return;
        }
        let values: Vec<Option<Loudness>> = measured.iter().map(|(_, loudness)| *loudness).collect();
        let Some(album) = ReplayGainTags::for_album(&values) else {
            info!("No album gain for '{album_title}': not every track could be measured");
            return;
        };
        for (path, _) in &measured {
            if !super::file_exists(path) {
                continue;
            }
            if let Err(e) = write_album_gain(Path::new(path), album.gain_db, album.peak) {
                warn!("Could not write the album gain to {path}: {e}");
            }
        }
        info!(
            "Album gain {} (peak {}) written to {} tracks of '{album_title}'",
            album.gain_text(),
            album.peak_text(),
            measured.len()
        );
    }
}

/// A source's own album tag beats nothing, and beats filing the track under its title. Its
/// compilation flag counts only for that album: a song the chooser filed elsewhere is not a
/// compilation because the file it came from was ripped from one.
fn fill_album_from_file(song: &mut Song, file_path: &str) {
    let (album, album_artist, compilation) = tag_writer_extras::read_album(Path::new(file_path));
    let album_blank = album.as_deref().is_none_or(dotnet::is_blank);
    let same_album = dotnet::is_blank(&song.album)
        || album_blank
        || SongIdentity::key(&song.album) == SongIdentity::key(album.as_deref().unwrap_or(""));
    if same_album && (compilation || BaseDownloadService::is_various_artists(album_artist.as_deref())) {
        song.is_compilation = true;
    }
    if !dotnet::is_blank(&song.album) || album_blank {
        return;
    }
    song.album = album.as_deref().unwrap_or("").trim().to_string();
    if song.album_artist.as_deref().is_none_or(str::is_empty)
        && let Some(album_artist) = album_artist.as_deref().filter(|a| !dotnet::is_blank(a))
    {
        song.album_artist = Some(album_artist.trim().to_string());
    }
}

/// Drop a redundant leading "Artist - " from a track title.
fn strip_artist_prefix(artist: &str, title: &str) -> String {
    let t = title.trim();
    let a = artist.trim();
    let prefix = format!("{a} - ");
    if !a.is_empty() && dotnet::starts_with_ignore_case(t, &prefix) {
        // OrdinalIgnoreCase matches one character for one.
        return t
            .chars()
            .skip(prefix.chars().count())
            .collect::<String>()
            .trim()
            .to_string();
    }
    t.to_string()
}

/// `BaseDownloadService.FillBlanksFromCatalog`, for the tag preview.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatalogBlanks;

impl CatalogBlankFiller for CatalogBlanks {
    fn fill_blanks_from_catalog(&self, song: &mut Song, meta: Option<&FullTrackMeta>) {
        BaseDownloadService::fill_blanks_from_catalog(song, meta);
    }
}

/// The loudness meter as the tag preview asks it: the measurement read through
/// `ReplayGainTags.ForTrack`.
pub struct MeterPreview(pub Arc<dyn ILoudnessMeter>);

#[async_trait]
impl PreviewLoudnessMeter for MeterPreview {
    async fn measure(
        &self,
        path: &str,
        timeout_seconds: i32,
        ct: &CancellationToken,
    ) -> anyhow::Result<Option<MeasuredLoudness>> {
        let measured = self.0.measure(Path::new(path), timeout_seconds, ct).await;
        Ok(measured.map(|loudness| MeasuredLoudness {
            integrated_lufs: loudness.integrated_lufs,
            true_peak_dbfs: loudness.true_peak_dbfs,
            replay_gain: ReplayGainTags::for_track(Some(&loudness)).map(|tags| ReplayGainText {
                gain_text: tags.gain_text(),
                peak_text: tags.peak_text(),
            }),
        }))
    }
}
