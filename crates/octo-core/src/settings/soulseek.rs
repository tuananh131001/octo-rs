//! `Octo.Models.Settings.SoulseekSettings` (the `Soulseek` section).

use serde::{Deserialize, Serialize};

/// Configuration for the Soulseek (slskd) integration.
/// Octo talks to a self-hosted slskd instance which fronts the Soulseek P2P network.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SoulseekSettings {
    /// Base URL of the slskd REST API (e.g. http://slskd:5030 when running in the same docker network).
    pub base_url: Option<String>,

    /// slskd web UI / API admin username (Basic Auth).
    pub username: Option<String>,

    /// slskd web UI / API admin password (Basic Auth).
    pub password: Option<String>,

    /// How long to wait (seconds) for a Soulseek search to gather peer responses
    /// before returning results. Soulseek searches stream in over time, so this is
    /// the difference between finding a lossless file and silently settling for a
    /// transcode.
    ///
    /// This was 6, which measurement showed is simply too short: polling slskd's
    /// /responses for the same query returned nothing at 6s, 10s or 15s, then 14
    /// responses including 5 FLACs at 20s — reproducibly, across three runs. The
    /// effect was that every star fell back to YouTube MP3 while lossless copies
    /// were sitting there unseen. Note that the search status object reports a
    /// responseCount well before /responses will hand the files over, so a status
    /// poll makes short waits look adequate when they are not.
    ///
    /// This is a CEILING, not a duration. slskd ends most searches itself, after 15 s
    /// with no new answer or at the response or file limit, and Octo reads the answers
    /// then. A search still running at the ceiling is cancelled, which makes slskd hand
    /// over everything it gathered, so a long search costs time but never its results.
    ///
    /// Star-triggered downloads are fire-and-forget, so the wait costs the user
    /// nothing; it only delays the file landing.
    pub search_wait_seconds: i32,

    /// The search ceiling for Better quality and the weekly upgrade. Longer than SearchWaitSeconds
    /// because these searches exist for songs the quick search did not find.
    /// Environment variable: SLSKD_UPGRADE_SEARCH_WAIT_SECONDS
    pub upgrade_search_wait_seconds: i32,

    /// Minimum file size in bytes to consider a search hit a real lossless file.
    /// Default 5 MB filters out 30s teaser clips and mislabelled tiny files.
    pub min_file_size_bytes: i64,

    /// Preferred file extension. Hits with this extension are sorted first.
    pub preferred_extension: String,

    /// How long (seconds) a download may go without a new byte before Octo gives up on
    /// that peer, cancels the transfer in slskd and tries the next one. A peer that
    /// keeps sending is waited for however slow it is, up to an hour. Per attempt, not
    /// per track. A peer that rejects outright is detected in seconds.
    pub download_timeout_seconds: i32,

    /// Fingerprint every finished download with Chromaprint and ask AcoustID what it actually
    /// is before accepting it. A Soulseek file identified as a different recording is discarded
    /// and the next peer tried; a YouTube file has no second candidate, so it is kept and, with
    /// the Review playlist on, asked about. Off by default: it needs a free AcoustID key and
    /// the fpcalc binary, and without both it can only ever be a no-op.
    ///
    /// This also switches on the rejected-peer memory. A file discarded for being the wrong
    /// recording, by this check OR by the pre-existing duration check, is remembered by peer
    /// and filename and never offered as a candidate again.
    /// Environment variable: SLSKD_VERIFY_DOWNLOADS
    pub verify_downloads: bool,

    /// AcoustID application key, free from https://acoustid.org/new-application. Blank
    /// disables the lookup half of VerifyDownloads; the duration check and its rejection
    /// memory keep working without it. Named ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_API_KEY
    pub acoust_id_api_key: String,

    /// On a confident AcoustID match, write that recording's title, artist, album and year,
    /// which are MusicBrainz's, onto the file instead of trusting the source's tags. Applies to
    /// Soulseek and YouTube downloads alike. Does nothing unless VerifyDownloads is on and a key
    /// is set.
    /// Environment variable: SLSKD_TAG_FROM_MUSICBRAINZ
    pub tag_from_music_brainz: bool,

    /// On a confident AcoustID match, name the file from the matched recording as well as
    /// tagging it: artist folder, title, album and track number all come from MusicBrainz, so
    /// the path and the tags are one decision (#48). Implies TagFromMusicBrainz for that file,
    /// because a path from MusicBrainz beside tags from the source is the split this removes.
    /// Off by default: a canonical name is not always the one a user wants on disk (a legal
    /// name, or a composer where the library files the performer). Only files Octo downloads
    /// and confirms are affected; nothing already in the library is renamed.
    /// Environment variable: NAME_FROM_MATCH
    pub name_from_match: bool,

    /// Minimum AcoustID fingerprint score, as a percentage, before a lookup result is
    /// allowed to decide anything at all.
    ///
    /// Counter-intuitively this is a permissiveness dial, not a strictness dial: results
    /// below it are ignored entirely rather than treated as rejections, so a HIGHER value
    /// rejects FEWER files. The admin help text says so out loud because the intuition runs
    /// the other way.
    /// Environment variable: SLSKD_MIN_MATCH_SCORE
    pub min_match_score: i32,

    /// How long a rejected peer and filename is remembered. Long enough that a wrong file is
    /// not re-fetched across a listening season, short enough that a verdict that was simply
    /// wrong lapses without the user ever learning the file existed. 0 never forgets, which
    /// suits anyone who would rather clear the list by hand.
    /// Environment variable: SLSKD_REJECTED_PEER_DAYS
    pub rejected_peer_ttl_days: i32,

    /// How many seconds of audio to fingerprint. AcoustID's own tools submit 120, and more
    /// costs decode time without identifying anything extra.
    /// Environment variable: SLSKD_FINGERPRINT_SECONDS
    pub fingerprint_seconds: i32,

    /// How long fpcalc may take before it is treated as hung. A FLAC fingerprints in about a
    /// second, so thirty is a hang rather than a slow disk; raise it for very slow storage.
    /// Environment variable: SLSKD_FINGERPRINT_TIMEOUT_SECONDS
    pub fingerprint_timeout_seconds: i32,

    /// How long an AcoustID lookup may take. Verification sits between a finished transfer and
    /// the file joining the library, so a slow AcoustID must cost seconds, never the download.
    /// Environment variable: SLSKD_ACOUSTID_TIMEOUT_SECONDS
    pub acoust_id_timeout_seconds: i32,

    /// Check a download that claims to be lossless (FLAC, WAV, AIFF and the like) for a lossy
    /// file converted to it, by the cutoff in its spectrum. A likely transcode is passed over
    /// for another lossless copy when there is one, and kept, marked as transcoded, when there
    /// is not: it is still the right song. Needs no API key and never fails a download; a
    /// missing ffmpeg or a file it cannot read is no opinion. On by default, since it costs a
    /// second or so of decoding per lossless download and only ever changes which copy is kept.
    /// Also used by the better-quality library action and the duplicate scan.
    pub detect_transcodes: bool,

    /// How long the transcode check may decode for, every window included. A FLAC takes well
    /// under a second, so twenty is a hang rather than a slow disk.
    pub transcode_check_timeout_seconds: i32,

    /// How long a Soulseek-first download waits while slskd is not logged in to the Soulseek
    /// network, before it goes to the next source. slskd answering is not slskd being able to
    /// search: during Soulseek's maintenance on 2026-10-03 it answered for three hours while
    /// every search failed, and a hearted album landed as YouTube MP3s. Six hours covers a
    /// normal maintenance window. 0 turns the wait off. Read live, no restart needed.
    /// Environment variable: SLSKD_OUTAGE_HOLD_HOURS
    pub outage_hold_hours: i32,

    /// How many downloads may transfer at once. Each Soulseek download lands in its own folder,
    /// which is what makes this safe; until slskd has put one there, Octo keeps to one at a time
    /// whatever this says. Placing files into the library stays one at a time either way. 1 is the
    /// old behaviour exactly. Read live.
    /// Environment variable: SLSKD_PARALLEL_DOWNLOADS
    pub parallel_downloads: i32,

    /// An album heart searches the album once and takes one peer's folder of it in one batch, then
    /// searches song by song only for what that folder lacks. Off is the old song by song walk.
    /// Environment variable: SLSKD_ALBUM_FOLDERS
    pub album_folders: bool,

    /// Send AcoustID the fingerprints a person confirmed with Keep, so the next person who
    /// downloads that recording gets Confirmed instead of Inconclusive (#47). Only a fingerprint
    /// whose MusicBrainz recording is unambiguous, only after a human kept it, and never one
    /// AcoustID confidently called something else. Needs AcoustIdUserApiKey. Off by default: it
    /// writes to a public database.
    /// Environment variable: ACOUSTID_SUBMIT
    pub submit_confirmed_fingerprints: bool,

    /// Your personal AcoustID key, shown at acoustid.org after signing in. Separate from the
    /// application key, because AcoustID credits a submission to a person, not an app. Named
    /// ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_USER_KEY
    pub acoust_id_user_api_key: String,
}

impl Default for SoulseekSettings {
    fn default() -> Self {
        Self {
            base_url: None,
            username: None,
            password: None,
            search_wait_seconds: 30,
            upgrade_search_wait_seconds: 90,
            min_file_size_bytes: 5 * 1024 * 1024,
            preferred_extension: "flac".to_string(),
            download_timeout_seconds: 180,
            verify_downloads: false,
            acoust_id_api_key: String::new(),
            tag_from_music_brainz: false,
            name_from_match: false,
            min_match_score: 85,
            rejected_peer_ttl_days: 30,
            fingerprint_seconds: 120,
            fingerprint_timeout_seconds: 30,
            acoust_id_timeout_seconds: 10,
            detect_transcodes: true,
            transcode_check_timeout_seconds: 20,
            outage_hold_hours: 6,
            parallel_downloads: 3,
            album_folders: true,
            submit_confirmed_fingerprints: false,
            acoust_id_user_api_key: String::new(),
        }
    }
}

impl SoulseekSettings {
    /// 0 means never forget, so it is not clamped upward.
    pub fn effective_rejected_peer_ttl_days(&self) -> i32 {
        if self.rejected_peer_ttl_days <= 0 {
            0
        } else {
            self.rejected_peer_ttl_days.clamp(1, 3650)
        }
    }

    pub fn effective_fingerprint_seconds(&self) -> i32 {
        self.fingerprint_seconds.clamp(15, 600)
    }

    pub fn effective_fingerprint_timeout_seconds(&self) -> i32 {
        self.fingerprint_timeout_seconds.clamp(5, 300)
    }

    pub fn effective_acoust_id_timeout_seconds(&self) -> i32 {
        self.acoust_id_timeout_seconds.clamp(2, 120)
    }

    pub fn effective_transcode_check_timeout_seconds(&self) -> i32 {
        self.transcode_check_timeout_seconds.clamp(5, 300)
    }

    pub fn effective_upgrade_search_wait_seconds(&self) -> i32 {
        self.upgrade_search_wait_seconds.clamp(30, 300)
    }

    /// Two days at most: past that the next source is the better answer.
    pub fn effective_outage_hold_hours(&self) -> i32 {
        self.outage_hold_hours.clamp(0, 48)
    }

    /// Six at most: more peers at once gains little and spends the Soulseek network's patience.
    pub fn effective_parallel_downloads(&self) -> i32 {
        self.parallel_downloads.clamp(1, 6)
    }

    /// Below 50 an AcoustID score is noise and acting on it manufactures false rejections;
    /// 100 is a score no real fingerprint reaches, which would silently disable the feature.
    pub fn effective_min_match_score(&self) -> i32 {
        self.min_match_score.clamp(50, 99)
    }

    /// The same threshold as AcoustID reports it, in 0..1.
    pub fn effective_min_score_fraction(&self) -> f64 {
        f64::from(self.effective_min_match_score()) / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ParallelDownloadTests.TheSettingIsClamped
    #[test]
    fn the_setting_is_clamped() {
        let with = |n| SoulseekSettings {
            parallel_downloads: n,
            ..Default::default()
        };
        assert_eq!(with(50).effective_parallel_downloads(), 6);
        assert_eq!(with(0).effective_parallel_downloads(), 1);
        assert_eq!(SoulseekSettings::default().parallel_downloads, 3);
    }

    #[test]
    fn rejected_peer_ttl_treats_zero_as_forever() {
        for (configured, expected) in [(0, 0), (-3, 0), (30, 30), (99999, 3650)] {
            let s = SoulseekSettings {
                rejected_peer_ttl_days: configured,
                ..Default::default()
            };
            assert_eq!(s.effective_rejected_peer_ttl_days(), expected, "{configured}");
        }
    }

    #[test]
    fn match_score_is_clamped_and_read_as_a_fraction() {
        let s = SoulseekSettings {
            min_match_score: 100,
            ..Default::default()
        };
        assert_eq!(s.effective_min_match_score(), 99);
        assert_eq!(SoulseekSettings::default().effective_min_score_fraction(), 0.85);
    }
}
