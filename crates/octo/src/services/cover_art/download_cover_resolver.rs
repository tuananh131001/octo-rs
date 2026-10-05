//! Port of `Services/CoverArt/DownloadCoverResolver.cs`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use octo_core::fingerprint::VerificationResult;
use octo_core::models::domain::Song;
use octo_core::settings::SettingsStore;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_media::cover::cover_image;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{CoverArtAggregator, CoverArtArchiveLookup, ITunesCoverArtLookup};

/// The cover a download gets, where it came from, and whether it is simply the one the file
/// already carries (so there is nothing to rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverChoice {
    pub bytes: Vec<u8>,
    pub source: String,
    pub keeps_existing: bool,
}

impl CoverChoice {
    pub fn new(bytes: Vec<u8>, source: &str, keeps_existing: bool) -> Self {
        Self {
            bytes,
            source: source.to_string(),
            keeps_existing,
        }
    }
}

/// The download-time cover chain (#51). The download path used to embed one Deezer URL and stop,
/// so anything Deezer did not know was written with no art, while the aggregator that already
/// knew iTunes and Last.fm sat unused beside it.
///
/// Asked in order: Apple's master of the same album (often 3000 px, and only on a strict match),
/// the Cover Art Archive when a fingerprint named the release the album tag describes, the
/// catalog's own cover, the aggregator by name, and last the file's own art. The first one at
/// least [`Self::SHARP_SIDE`] wide wins at once; otherwise the largest one seen does. Taking the
/// first usable one let a 500 px archive scan or a peer's 200 px thumbnail beat the catalog's
/// 1000 px cover. A cover that is not square counts as missing, and a letterboxed video frame
/// gives up its centre only when nothing else was found.
pub struct DownloadCoverResolver {
    archive: Arc<CoverArtArchiveLookup>,
    aggregator: Arc<CoverArtAggregator>,
    /// `IHttpClientFactory.CreateClient()`: the default client, for the catalog's cover URL.
    http: reqwest::Client,
    /// `IOptionsMonitor<MetadataSettings>`: read at every resolve.
    settings: Arc<SettingsStore>,
    itunes: Option<Arc<ITunesCoverArtLookup>>,
}

/// The best cover offered so far and its shorter side.
#[derive(Default)]
struct Best {
    choice: Option<CoverChoice>,
    side: u32,
}

impl Best {
    /// True when this one is sharp enough to stop; otherwise it is kept if it is the biggest.
    fn offer(
        &mut self,
        bytes: Option<&[u8]>,
        source: &str,
        keeps_existing: bool,
        require_square: bool,
    ) -> bool {
        let Some(bytes) = bytes else {
            return false;
        };
        if !cover_image::is_usable(Some(bytes), require_square) {
            return false;
        }
        let Some((width, height)) = cover_image::measure(bytes) else {
            return false;
        };
        let side = width.min(height);
        if side > self.side {
            self.choice = Some(CoverChoice::new(bytes.to_vec(), source, keeps_existing));
            self.side = side;
        }
        side >= DownloadCoverResolver::SHARP_SIDE
    }
}

impl DownloadCoverResolver {
    /// A slow cover source must cost seconds, never the download: the whole finalize phase runs
    /// under the download lock.
    const CATALOG_TIMEOUT: Duration = Duration::from_secs(8);

    /// Big enough to stop looking: the catalog's own covers are 1000 px. Apple's master, asked
    /// first, is usually far larger, and is what a match there gets.
    pub const SHARP_SIDE: u32 = 1000;

    pub fn new(
        archive: Arc<CoverArtArchiveLookup>,
        aggregator: Arc<CoverArtAggregator>,
        http: reqwest::Client,
        settings: Arc<SettingsStore>,
        itunes: Option<Arc<ITunesCoverArtLookup>>,
    ) -> Self {
        Self {
            archive,
            aggregator,
            http,
            settings,
            itunes,
        }
    }

    pub async fn resolve(
        &self,
        song: &Song,
        embedded: Option<&[u8]>,
        ct: &CancellationToken,
    ) -> Option<CoverChoice> {
        tokio::select! {
            biased;
            _ = ct.cancelled() => None,
            choice = self.resolve_inner(song, embedded) => choice,
        }
    }

    async fn resolve_inner(&self, song: &Song, embedded: Option<&[u8]>) -> Option<CoverChoice> {
        let settings = self.settings.current().metadata.clone();
        let require_square = settings.replace_video_covers;
        let mut best = Best::default();
        let artist = song.primary_artist.as_deref().unwrap_or(&song.artist);

        // A compilation's album artist is nobody Apple would list it under.
        if let Some(itunes) = &self.itunes
            && !song.is_compilation
        {
            // A barcode the chooser found names one release outright, so Apple is asked by it
            // first and the master lookup below answers from that match without a search.
            if let Some(barcode) = song.barcode.as_deref().filter(|b| !b.is_empty())
                && !dotnet::is_blank(&song.album)
            {
                itunes
                    .prime_by_barcode(
                        &[(artist.to_string(), song.album.clone(), barcode.to_string())],
                        None,
                    )
                    .await;
            }
            let master = itunes
                .try_fetch_album_master(Some(artist), Some(&song.album), Some(&song.title))
                .await;
            if best.offer(master.as_deref(), "iTunes", false, require_square) {
                return best.choice;
            }
        }

        // Only when the album tag IS the release the fingerprint matched: a download tagged with
        // a compilation's name must not get the original album's cover.
        let named = |id: &Option<String>| id.as_deref().is_some_and(|id| !id.is_empty());
        if settings.use_cover_art_archive
            && (named(&song.music_brainz_release_id) || named(&song.music_brainz_release_group_id))
            && VerificationResult::album_is_from_release(song)
        {
            let archived = self
                .archive
                .try_fetch(
                    song.music_brainz_release_id.as_deref(),
                    song.music_brainz_release_group_id.as_deref(),
                )
                .await;
            if best.offer(archived.as_deref(), "Cover Art Archive", false, require_square) {
                return best.choice;
            }
        }

        if let Some(url) = song
            .cover_art_url_large
            .as_deref()
            .or(song.cover_art_url.as_deref())
            .filter(|url| !url.is_empty())
        {
            let catalog = self.download(url).await;
            if best.offer(catalog.as_deref(), "the catalog", false, require_square) {
                return best.choice;
            }
        }

        let routing = SoulseekRouting {
            kind: if dotnet::is_blank(&song.album) {
                RoutingKind::Song
            } else {
                RoutingKind::Album
            },
            artist: Some(artist.to_string()),
            title: Some(song.title.clone()),
            album: Some(song.album.clone()),
            ..Default::default()
        };
        // The aggregator never fails; the C# caught what a source might throw here.
        let aggregated = self.aggregator.get_cover(&routing, true).await;
        if best.offer(aggregated.as_deref(), "a cover search", false, require_square) {
            return best.choice;
        }

        if let Some(embedded) = embedded.filter(|e| !e.is_empty()) {
            best.offer(Some(embedded), "the file itself", true, require_square);
        }
        if let Some(choice) = best.choice {
            if best.side < Self::SHARP_SIDE {
                info!(
                    "Best cover for {} - {} is {} px, from {}",
                    song.artist, song.title, best.side, choice.source
                );
            }
            return Some(choice);
        }

        let embedded = embedded.filter(|e| !e.is_empty())?;
        if !require_square {
            return None;
        }
        let cropped = cover_image::crop_to_square(embedded)?;
        cover_image::is_usable(Some(&cropped), true)
            .then(|| CoverChoice::new(cropped, "the centre of a video frame", false))
    }

    async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let attempt = async {
            let response = self.http.get(url).send().await?;
            if !response.status().is_success() {
                return anyhow::Ok(None);
            }
            Ok(Some(response.bytes().await?.to_vec()))
        };
        match tokio::time::timeout(Self::CATALOG_TIMEOUT, attempt).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                debug!("cover download {url} failed: {error}");
                None
            }
            Err(_) => {
                debug!("cover download {url} failed: The operation was canceled.");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "download_cover_resolver_tests.rs"]
mod tests;
