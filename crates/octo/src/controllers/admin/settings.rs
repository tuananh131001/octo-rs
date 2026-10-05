//! `AdminController`'s settings actions (L678–L1136): `GET /api/admin/settings` (the
//! effective configuration the forms pre-fill from) and `POST /api/admin/settings` (a partial
//! patch merged into settings.json), with the validation and the secret placeholder rules the
//! save path applies. The effective document is shared with `GET raw-config`.

use std::fmt::Debug;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::common::dotnet::{is_blank, ordinal_ignore_case_key, utf16_len};
use octo_core::json::dom::Node;
use octo_core::settings::{
    GenreMatchMode, JsonObject, LastFmUserSession, LibraryActionSettings, LyricsSaveTo, SettingsWriteError,
};
use std::collections::HashSet;
use tracing::{error, info, warn};

use super::helpers_6b1::{
    SECRET_PLACEHOLDER, body_text, child, child_mut, error_json, json_node, key_of, mask_secret, n_num,
    n_obj, n_str, n_str_array, parse_json_node,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};

/// An enum as `.ToString()` wrote it: the member's name.
fn name<T: Debug>(value: T) -> Node {
    n_str(&format!("{value:?}"))
}

/// `JsonSerializer.SerializeToNode(genre.Mappings)`: the class as STJ wrote it, `Match` as
/// its number.
fn genre_mappings_node(mappings: &[octo_core::settings::GenreMappingSettings]) -> Node {
    Node::Array(
        mappings
            .iter()
            .map(|m| {
                n_obj([
                    ("Id", n_str(&m.id)),
                    ("Pattern", n_str(&m.pattern)),
                    ("Genre", n_str(&m.genre)),
                    (
                        "Match",
                        n_num(match m.match_mode {
                            GenreMatchMode::Contains => 0,
                            GenreMatchMode::Exact => 1,
                        }),
                    ),
                    ("Enabled", Node::Bool(m.enabled)),
                ])
            })
            .collect(),
    )
}

/// `JsonSerializer.SerializeToNode(lastfm.DiscoveryStations)`.
fn discovery_stations_node(stations: &[octo_core::settings::DiscoveryStationSettings]) -> Node {
    Node::Array(
        stations
            .iter()
            .map(|s| {
                n_obj([
                    ("Id", n_str(&s.id)),
                    ("Name", n_str(&s.name)),
                    ("Enabled", Node::Bool(s.enabled)),
                    ("Tags", n_str_array(&s.tags)),
                ])
            })
            .collect(),
    )
}

/// `MaskSessions`: each listener's Last.fm link with the key masked, enough for the Raw editor
/// to round-trip it and for nobody to read it back. Entries with a blank key are left out.
pub fn mask_sessions<'a>(sessions: impl IntoIterator<Item = (&'a String, &'a LastFmUserSession)>) -> Node {
    Node::Object(
        sessions
            .into_iter()
            .filter(|(_, session)| !is_blank(&session.session_key))
            .map(|(user, session)| {
                (
                    user.clone(),
                    n_obj([
                        ("SessionKey", n_str(SECRET_PLACEHOLDER)),
                        ("LastFmUser", n_str(&session.last_fm_user)),
                    ]),
                )
            })
            .collect(),
    )
}

/// The *effective* configuration the app sees right now, as GET settings and GET raw-config
/// both build it: every section, PascalCase keys (a `Dictionary`/`JsonObject`, so no camelCase),
/// enums by name, and the admin password, Last.fm secret and session keys as the placeholder.
/// `raw_config` selects raw-config's one difference: `DiscoveryStations` is always the class
/// serialised, never the file's.
pub fn effective_document(state: &AppState, raw_config: bool) -> JsonObject {
    let current = state.settings.current();
    let subsonic = &current.subsonic;
    let soulseek = &current.soulseek;
    let lidarr = &current.lidarr;
    let lastfm = &current.last_fm;
    let genre = &current.genre;
    let actions = &current.library_actions;
    let mixes = &current.generated_playlists;
    let notif = &current.notifications;
    let metadata = &current.metadata;
    let listen_brainz = &current.listen_brainz;
    let file = state.settings_writer.load();
    let opt = |v: &Option<String>| n_str(v.as_deref().unwrap_or(""));

    let mut doc = JsonObject::new();
    doc.insert(
        "Subsonic".into(),
        n_obj([
            ("Url", opt(&subsonic.url)),
            // Both are fields on the Music server form. Missing here, they never pre-filled, so
            // every save of that form wrote empty strings over them and quietly took away the
            // admin identity library actions and authenticated rescans depend on. The password
            // goes out as a placeholder the save swaps back, so it never reaches a browser.
            ("AdminUsername", opt(&subsonic.admin_username)),
            (
                "AdminPassword",
                n_str(&mask_secret(subsonic.admin_password.as_deref())),
            ),
            (
                "EnableSearchDiscovery",
                Node::Bool(subsonic.enable_search_discovery),
            ),
            (
                "WaitForSearchDurations",
                Node::Bool(subsonic.wait_for_search_durations),
            ),
            ("EnableSyncCatalog", Node::Bool(subsonic.enable_sync_catalog)),
            ("SyncCatalogClients", n_str(&subsonic.sync_catalog_clients)),
            ("SyncCatalogMaxSongs", n_num(subsonic.sync_catalog_max_songs)),
            ("StorageMode", name(subsonic.storage_mode)),
            ("DownloadMode", name(subsonic.download_mode)),
            ("DownloadOnStar", Node::Bool(subsonic.download_on_star)),
            ("DownloadAlbumOnStar", Node::Bool(subsonic.download_album_on_star)),
            ("RecordRequestedBy", Node::Bool(subsonic.record_requested_by)),
            (
                "StarDownloadsForRequester",
                Node::Bool(subsonic.star_downloads_for_requester),
            ),
            ("SkipOwnedSongs", Node::Bool(subsonic.skip_owned_songs)),
            (
                "WaitForLosslessOnPlay",
                Node::Bool(subsonic.wait_for_lossless_on_play),
            ),
            (
                "LosslessWaitTimeoutSeconds",
                n_num(subsonic.lossless_wait_timeout_seconds),
            ),
            ("DownloadOnPlay", Node::Bool(subsonic.download_on_play)),
            ("LidarrAlbumOnPlay", Node::Bool(subsonic.lidarr_album_on_play)),
            // These two are rendered by the dashboard but were missing here, so their fields
            // never pre-filled with the saved value.
            ("DownloadSource", name(subsonic.download_source)),
            (
                "HeartDownloadSources",
                Node::Array(
                    subsonic
                        .effective_heart_download_sources()
                        .iter()
                        .map(|step| {
                            n_obj([
                                ("Source", name(step.source)),
                                ("SongEnabled", Node::Bool(step.song_enabled == Some(true))),
                                ("AlbumEnabled", Node::Bool(step.album_enabled == Some(true))),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "AutoDetectDownloadPath",
                Node::Bool(subsonic.auto_detect_download_path),
            ),
            ("LibraryPath", n_str(&subsonic.library_path)),
            ("FolderStructure", name(subsonic.folder_structure)),
            ("UseLocalStaging", Node::Bool(subsonic.use_local_staging)),
            ("ExplicitFilter", name(subsonic.explicit_filter)),
            ("CacheDurationHours", n_num(subsonic.cache_duration_hours)),
            (
                "EnableExternalPlaylists",
                Node::Bool(subsonic.enable_external_playlists),
            ),
            ("PlaylistsDirectory", n_str(&subsonic.playlists_directory)),
        ]),
    );
    doc.insert(
        "Library".into(),
        n_obj([(
            "DownloadPath",
            n_str(
                &state
                    .settings
                    .raw("Library:DownloadPath")
                    .unwrap_or_else(|| "/music".to_string()),
            ),
        )]),
    );
    // Must be listed even though nothing reads it back: PUT raw-config writes this document
    // wholesale, so a section missing from the GET is a section the next plain Save deletes.
    doc.insert(
        "Server".into(),
        n_obj([("PublicUrl", n_str(&current.server.public_url))]),
    );
    // The Updates section as configured now; read when asked, so a save shows at once.
    doc.insert(
        "Updates".into(),
        n_obj([
            ("Check", Node::Bool(current.updates.check)),
            ("Repo", n_str(&current.updates.repo)),
        ]),
    );
    doc.insert(
        "Soulseek".into(),
        n_obj([
            ("BaseUrl", opt(&soulseek.base_url)),
            ("Username", opt(&soulseek.username)),
            ("Password", opt(&soulseek.password)),
            ("SearchWaitSeconds", n_num(soulseek.search_wait_seconds)),
            (
                "UpgradeSearchWaitSeconds",
                n_num(soulseek.upgrade_search_wait_seconds),
            ),
            ("MinFileSizeBytes", n_num(soulseek.min_file_size_bytes)),
            ("PreferredExtension", n_str(&soulseek.preferred_extension)),
            ("DownloadTimeoutSeconds", n_num(soulseek.download_timeout_seconds)),
            ("VerifyDownloads", Node::Bool(soulseek.verify_downloads)),
            ("AcoustIdApiKey", n_str(&soulseek.acoust_id_api_key)),
            ("MinMatchScore", n_num(soulseek.min_match_score)),
            ("TagFromMusicBrainz", Node::Bool(soulseek.tag_from_music_brainz)),
            ("NameFromMatch", Node::Bool(soulseek.name_from_match)),
            ("RejectedPeerTtlDays", n_num(soulseek.rejected_peer_ttl_days)),
            ("FingerprintSeconds", n_num(soulseek.fingerprint_seconds)),
            (
                "FingerprintTimeoutSeconds",
                n_num(soulseek.fingerprint_timeout_seconds),
            ),
            (
                "AcoustIdTimeoutSeconds",
                n_num(soulseek.acoust_id_timeout_seconds),
            ),
            ("DetectTranscodes", Node::Bool(soulseek.detect_transcodes)),
            (
                "TranscodeCheckTimeoutSeconds",
                n_num(soulseek.transcode_check_timeout_seconds),
            ),
            ("OutageHoldHours", n_num(soulseek.outage_hold_hours)),
            ("ParallelDownloads", n_num(soulseek.parallel_downloads)),
            ("AlbumFolders", Node::Bool(soulseek.album_folders)),
            (
                "SubmitConfirmedFingerprints",
                Node::Bool(soulseek.submit_confirmed_fingerprints),
            ),
            ("AcoustIdUserApiKey", n_str(&soulseek.acoust_id_user_api_key)),
        ]),
    );
    doc.insert(
        "Lidarr".into(),
        n_obj([
            ("BaseUrl", opt(&lidarr.base_url)),
            ("ApiKey", opt(&lidarr.api_key)),
            ("RootFolderPath", opt(&lidarr.root_folder_path)),
            ("QualityProfileId", n_num(lidarr.quality_profile_id)),
            ("MetadataProfileId", n_num(lidarr.metadata_profile_id)),
            ("CompletionMode", name(lidarr.completion_mode)),
            ("ImportTimeoutSeconds", n_num(lidarr.import_timeout_seconds)),
        ]),
    );
    doc.insert(
        "YouTube".into(),
        n_obj([(
            "ShimUrl",
            n_str(&state.settings.raw("YouTube:ShimUrl").unwrap_or_default()),
        )]),
    );
    let discovery_stations = if raw_config {
        discovery_stations_node(&lastfm.discovery_stations)
    } else {
        file.get("LastFm")
            .and_then(|section| section.get("DiscoveryStations"))
            .filter(|node| !matches!(node, Node::Null))
            .cloned()
            .unwrap_or_else(|| discovery_stations_node(&lastfm.discovery_stations))
    };
    doc.insert(
        "LastFm".into(),
        n_obj([
            ("ApiKey", n_str(&lastfm.api_key)),
            // Placeholders, which the saves swap back for what is stored.
            ("ApiSecret", n_str(&mask_secret(Some(&lastfm.api_secret)))),
            (
                "ScrobbleExternalPlays",
                Node::Bool(lastfm.scrobble_external_plays),
            ),
            ("ScrobbleLibraryPlays", Node::Bool(lastfm.scrobble_library_plays)),
            ("UserSessions", mask_sessions(&lastfm.user_sessions)),
            ("EnableRadio", Node::Bool(lastfm.enable_radio)),
            ("RadioTrackCount", n_num(lastfm.radio_track_count)),
            (
                "RadioCacheDurationHours",
                n_num(lastfm.radio_cache_duration_hours),
            ),
            (
                "EnablePersonalizedStations",
                Node::Bool(lastfm.enable_personalized_stations),
            ),
            ("EnableYourMix", Node::Bool(lastfm.enable_your_mix)),
            ("EnableDiscoveryMix", Node::Bool(lastfm.enable_discovery_mix)),
            ("ArtistStationCount", n_num(lastfm.artist_station_count)),
            ("GenreStationCount", n_num(lastfm.genre_station_count)),
            (
                "EnableDiscoveryStations",
                Node::Bool(lastfm.enable_discovery_stations),
            ),
            (
                "ExposeRadioAsPlaylists",
                Node::Bool(lastfm.expose_radio_as_playlists),
            ),
            ("ExposeRadioAsStreams", Node::Bool(lastfm.expose_radio_as_streams)),
            ("RadioStreamBitrateKbps", n_num(lastfm.radio_stream_bitrate_kbps)),
            ("EnableIcyMetadata", Node::Bool(lastfm.enable_icy_metadata)),
            (
                "StarterPublishTimeoutSeconds",
                n_num(lastfm.starter_publish_timeout_seconds),
            ),
            (
                "RadioLoudnessTargetLufs",
                n_num(lastfm.radio_loudness_target_lufs),
            ),
            ("HistoryRetentionDays", n_num(lastfm.history_retention_days)),
            ("DiscoveryPercent", n_num(lastfm.discovery_percent)),
            ("RefreshIntervalHours", n_num(lastfm.refresh_interval_hours)),
            ("MinimumPlays", n_num(lastfm.minimum_plays)),
            ("DiscoveryStations", discovery_stations),
        ]),
    );
    doc.insert(
        "LibraryActions".into(),
        n_obj([
            ("Enabled", Node::Bool(actions.enabled)),
            ("PlaylistsEnabled", Node::Bool(actions.playlists_enabled)),
            ("RatingsEnabled", Node::Bool(actions.ratings_enabled)),
            ("PlaylistPrefix", n_str(&actions.playlist_prefix)),
            ("DryRun", Node::Bool(actions.dry_run)),
            ("QuarantineDirectory", n_str(&actions.quarantine_directory)),
            (
                "QuarantineRetentionDays",
                n_num(actions.quarantine_retention_days),
            ),
            ("PollIntervalSeconds", n_num(actions.poll_interval_seconds)),
            ("MaxActionsPerCycle", n_num(actions.max_actions_per_cycle)),
            (
                "KeepReplacedOriginals",
                Node::Bool(actions.keep_replaced_originals),
            ),
            ("NoticePrefix", n_str(&actions.notice_prefix)),
            ("ReviewEnabled", Node::Bool(actions.review_enabled)),
            ("ReviewPlaylistName", n_str(&actions.review_playlist_name)),
            ("ReviewSweepPerHour", n_num(actions.review_sweep_per_hour)),
            (
                "ReviewSweepOctoDownloads",
                Node::Bool(actions.review_sweep_octo_downloads),
            ),
            ("DuplicatesEnabled", Node::Bool(actions.duplicates_enabled)),
            ("DuplicatesPlaylistName", n_str(&actions.duplicates_playlist_name)),
            ("DuplicatesScanHours", n_num(actions.duplicates_scan_hours)),
            ("UpgradePerWeek", n_num(actions.upgrade_per_week)),
            ("UpgradeSource", name(actions.upgrade_source)),
            ("NoticeMaxTracks", n_num(actions.notice_max_tracks)),
            ("RatingsScope", name(actions.ratings_scope)),
            ("AllowedUsers", n_str_array(&actions.allowed_users)),
            // Effective rather than raw, because the editor needs every action present even
            // when the config names only some of them. Projected so the enum lands as its NAME:
            // serialized directly it becomes a number, and the editor looks the action up by
            // name to label the row.
            (
                "Actions",
                Node::Array(
                    actions
                        .effective_actions()
                        .iter()
                        .map(|action| {
                            n_obj([
                                ("Action", name(action.action)),
                                ("Name", n_str(&action.name)),
                                ("Enabled", Node::Bool(action.enabled)),
                                ("Rating", action.rating.map_or(Node::Null, n_num)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
    );
    doc.insert(
        "Metadata".into(),
        n_obj([
            ("Language", n_str(&metadata.language)),
            ("AlbumFromTitle", Node::Bool(metadata.album_from_title)),
            ("UseCoverArtArchive", Node::Bool(metadata.use_cover_art_archive)),
            ("ReplaceVideoCovers", Node::Bool(metadata.replace_video_covers)),
            ("WriteCoverFile", Node::Bool(metadata.write_cover_file)),
            ("EmbedFullSizeCovers", Node::Bool(metadata.embed_full_size_covers)),
            ("FetchLyrics", Node::Bool(metadata.fetch_lyrics)),
            ("LyricsSources", n_str(&metadata.lyrics_sources)),
            (
                "PreferWordTimedLyrics",
                Node::Bool(metadata.prefer_word_timed_lyrics),
            ),
            (
                "WriteLyricsBesideAllSongs",
                Node::Bool(metadata.write_lyrics_beside_all_songs),
            ),
            (
                "SaveLyricsTo",
                n_str(LyricsSaveTo::normalize(Some(&metadata.save_lyrics_to))),
            ),
            ("PreferOriginalAlbum", Node::Bool(metadata.prefer_original_album)),
            (
                "YearFromOriginalRelease",
                Node::Bool(metadata.year_from_original_release),
            ),
            ("PreferredCountries", n_str(&metadata.preferred_countries)),
            (
                "ReleaseDetailsLookup",
                Node::Bool(metadata.release_details_lookup),
            ),
            ("ReplayGain", Node::Bool(metadata.replay_gain)),
            (
                "ReplayGainTimeoutSeconds",
                n_num(metadata.replay_gain_timeout_seconds),
            ),
            ("TagRehearsal", Node::Bool(metadata.tag_rehearsal)),
        ]),
    );
    doc.insert(
        "GeneratedPlaylists".into(),
        n_obj([
            ("Enabled", Node::Bool(mixes.enabled)),
            ("Genres", Node::Bool(mixes.genres)),
            ("Decades", Node::Bool(mixes.decades)),
            ("TrackCount", n_num(mixes.track_count)),
            ("MaxPerArtist", n_num(mixes.max_per_artist)),
            ("CreateAt", n_num(mixes.create_at)),
            ("RemoveBelow", n_num(mixes.remove_below)),
            ("MaxPlaylists", n_num(mixes.max_playlists)),
            ("RefreshHours", n_num(mixes.refresh_hours)),
            ("NewShare", n_num(mixes.new_share)),
            ("NewDays", n_num(mixes.new_days)),
            ("NameFormat", n_str(&mixes.name_format)),
        ]),
    );
    // Read from the raw file, for the same reason as DiscoveryStations: EffectiveMappings()
    // drops rows, and the editor has to show the user what they typed rather than what
    // survived.
    let mappings = file
        .get("Genre")
        .and_then(|section| section.get("Mappings"))
        .filter(|node| !matches!(node, Node::Null))
        .cloned()
        .unwrap_or_else(|| genre_mappings_node(&genre.mappings));
    doc.insert(
        "Genre".into(),
        n_obj([
            ("Enabled", Node::Bool(genre.enabled)),
            ("MaxGenres", n_num(genre.max_genres)),
            ("OnEmpty", name(genre.on_empty)),
            ("Fallback", name(genre.fallback)),
            ("UnknownLabel", n_str(&genre.unknown_label)),
            ("Blocklist", n_str_array(&genre.blocklist)),
            ("Mappings", mappings),
            (
                "BackfillMaxConsecutiveFailures",
                n_num(genre.backfill_max_consecutive_failures),
            ),
            ("BackfillExtensions", n_str_array(&genre.backfill_extensions)),
        ]),
    );
    doc.insert(
        "Notifications".into(),
        n_obj([
            ("NtfyUrl", n_str(&notif.ntfy_url)),
            ("NtfyToken", n_str(&notif.ntfy_token)),
            ("DiscordWebhookUrl", n_str(&notif.discord_webhook_url)),
            ("NotifyDownloadStarted", Node::Bool(notif.notify_download_started)),
            (
                "NotifyDownloadCompleted",
                Node::Bool(notif.notify_download_completed),
            ),
            (
                "NotifyLosslessFallback",
                Node::Bool(notif.notify_lossless_fallback),
            ),
            ("NotifyDownloadFailed", Node::Bool(notif.notify_download_failed)),
            ("NotifyAlbumCompleted", Node::Bool(notif.notify_album_completed)),
        ]),
    );
    // Present even when unset: a section missing from this document is a section the next
    // Raw Config save silently deletes.
    doc.insert(
        "ListenBrainz".into(),
        n_obj([
            ("Token", n_str(&listen_brainz.token)),
            (
                "SubmitExternalPlays",
                Node::Bool(listen_brainz.submit_external_plays),
            ),
            (
                "UserTokens",
                Node::Object(
                    listen_brainz
                        .user_tokens
                        .iter()
                        .map(|(user, token)| (user.clone(), n_str(token)))
                        .collect(),
                ),
            ),
        ]),
    );
    doc
}

/// `GET /api/admin/settings`: the *effective* configuration the app sees right now, so the UI
/// shows the same values code is using regardless of whether they came from an env var,
/// appsettings.json, or the editable settings file. Most secrets are returned in clear
/// because this admin endpoint is intended for trusted LAN-only access (as Navidrome's admin
/// pages are); the admin password, the Last.fm secret and the session keys are placeholders.
pub async fn get_settings(State(state): State<AppState>) -> Response {
    let mut doc = effective_document(&state, false);
    let path = state.settings_writer.file_path();
    let pending = state.restart_tracker.pending(&*state.settings);
    doc.insert(
        "_meta".into(),
        n_obj([
            ("ConfigFilePath", n_str(&path.display().to_string())),
            // Drives the "Forget rejected peers" button's label, so an empty list is visibly
            // empty rather than a button that looks like it did nothing.
            ("RejectedPeerCount", n_num(state.rejected_peers.count())),
            ("ConfigFileExists", Node::Bool(path.is_file())),
            // False when the file is there but unparseable, which is when saves are refused.
            ("ConfigFileValid", Node::Bool(state.settings_writer.is_readable())),
            // Restart-only settings whose saved value has not reached the running services.
            ("RestartPending", n_str_array(&pending)),
            // What a saved-but-hidden secret reads as, so the dashboard can recognise it.
            ("SecretPlaceholder", n_str(SECRET_PLACEHOLDER)),
            // So a bug report can name a build.
            ("Version", n_str(octo_core::VERSION)),
        ]),
    );
    json_node(StatusCode::OK, &Node::Object(doc))
}

// ---- JsonNode.GetValue<T>, which threw on a node of the wrong kind ----------------------------

fn wrong_kind(node: &Node, target: &str) -> AppError {
    let kind = match node {
        Node::Null => "Null",
        Node::Bool(true) => "True",
        Node::Bool(false) => "False",
        Node::Number(_) => "Number",
        Node::String(_) => "String",
        Node::Array(_) => "Array",
        Node::Object(_) => "Object",
    };
    AppError::InvalidOperation(format!(
        "An element of type '{kind}' cannot be converted to a '{target}'."
    ))
}

/// `node?.GetValue<bool>()`.
fn get_bool(node: Option<&Node>) -> Result<Option<bool>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(wrong_kind(other, "System.Boolean")),
    }
}

/// `node?.GetValue<string>()`.
fn get_string(node: Option<&Node>) -> Result<Option<&str>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::String(s)) => Ok(Some(s)),
        Some(other) => Err(wrong_kind(other, "System.String")),
    }
}

/// `node?.GetValue<int>()`.
fn get_int(node: Option<&Node>) -> Result<Option<i32>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::Number(text)) => text
            .parse::<i32>()
            .map(Some)
            .map_err(|_| wrong_kind(&Node::Number(text.clone()), "System.Int32")),
        Some(other) => Err(wrong_kind(other, "System.Int32")),
    }
}

/// `JsonValue.TryGetValue<string>`: the text of a string value, None for anything else.
pub fn string_value(node: Option<&Node>) -> Option<&str> {
    match node {
        Some(Node::String(s)) => Some(s),
        _ => None,
    }
}

/// The one rule with teeth: the dashboard must not be able to produce a live but unrestricted
/// configuration. An empty allowlist means nobody, so enabling the feature without naming
/// anyone is a mistake rather than a permissive choice.
pub fn validate_library_actions(
    actions: &JsonObject,
    current: &LibraryActionSettings,
) -> Result<Option<String>, AppError> {
    let enabled = get_bool(actions.get("Enabled"))?.unwrap_or(current.enabled);
    let allowed = match actions.get("AllowedUsers") {
        Some(Node::Array(items)) => Some(items),
        _ => None,
    };
    let allowed_count = match allowed {
        Some(items) => {
            let mut count = 0;
            for entry in items {
                if !get_string(Some(entry))?.is_none_or(is_blank) {
                    count += 1;
                }
            }
            count
        }
        None => current.allowed_users.len(),
    };

    if enabled && allowed_count == 0 {
        return Ok(Some(
            "Library actions need at least one allowed user. An empty list means nobody.".into(),
        ));
    }
    if allowed.is_some_and(|items| items.len() > 50) {
        return Ok(Some(
            "LibraryActions.AllowedUsers supports at most 50 entries".into(),
        ));
    }

    if let Some(Node::Array(definitions)) = actions.get("Actions") {
        if definitions.len() > 5 {
            return Ok(Some("There are only five library actions".into()));
        }
        let mut ratings = HashSet::new();
        for node in definitions {
            let Node::Object(definition) = node else {
                return Ok(Some("Every library action must be an object".into()));
            };
            let name = get_string(definition.get("Name"))?.map(str::trim).unwrap_or("");
            if utf16_len(name) > 80 {
                return Ok(Some(format!("'{name}' is longer than 80 characters")));
            }
            let rating = get_int(definition.get("Rating"))?.unwrap_or(0);
            if !(0..=5).contains(&rating) {
                return Ok(Some("A star rating must be between 0 and 5".into()));
            }
            if rating > 0 && !ratings.insert(rating) {
                return Ok(Some(format!(
                    "Two actions both use {rating} star(s); a rating can only mean one thing"
                )));
            }
        }
    }
    Ok(None)
}

/// `Enum.TryParse<GenreMatchMode>(value, ignoreCase: true, out _)`: a name in any case, a
/// number (defined or not), or a comma list of either.
fn parses_as_match_mode(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if value.parse::<i64>().is_ok() {
        return true;
    }
    value.split(',').all(|part| {
        let part = part.trim();
        part.eq_ignore_ascii_case("Contains") || part.eq_ignore_ascii_case("Exact")
    })
}

/// The POST path is untyped, so nothing checks a mapping table's shape unless this does.
/// Mirrors `validate_discovery_stations`, and is the second of three layers: the editor
/// validates in the browser and `effective_mappings()` sanitises again at read time.
pub fn validate_genre_mappings(mappings: &[Node]) -> Result<Option<String>, AppError> {
    if mappings.len() > 200 {
        return Ok(Some("Genre.Mappings supports at most 200 rules".into()));
    }
    let mut patterns = HashSet::new();
    for node in mappings {
        let Node::Object(rule) = node else {
            return Ok(Some("Every genre rule must be an object".into()));
        };
        let pattern = get_string(rule.get("Pattern"))?.map(str::trim).unwrap_or("");
        let genre = get_string(rule.get("Genre"))?.map(str::trim).unwrap_or("");
        let match_mode = get_string(rule.get("Match"))?
            .map(str::trim)
            .unwrap_or("Contains");

        let length = utf16_len(pattern);
        if length == 0 || length > 60 {
            return Ok(Some(
                "Every genre rule needs a pattern of at most 60 characters".into(),
            ));
        }
        if !patterns.insert(ordinal_ignore_case_key(pattern)) {
            return Ok(Some(format!(
                "Duplicate genre rule pattern '{pattern}': only the first could ever fire"
            )));
        }
        if utf16_len(genre) > 60 {
            return Ok(Some(format!(
                "The genre for '{pattern}' must be at most 60 characters"
            )));
        }
        if !parses_as_match_mode(match_mode) {
            return Ok(Some(format!(
                "The match mode for '{pattern}' must be Contains or Exact"
            )));
        }
    }
    Ok(None)
}

pub fn validate_discovery_stations(stations: &[Node]) -> Result<Option<String>, AppError> {
    if stations.len() > 12 {
        return Ok(Some(
            "LastFm.DiscoveryStations supports at most 12 entries".into(),
        ));
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for node in stations {
        let Node::Object(station) = node else {
            return Ok(Some("Every discovery station must be an object".into()));
        };
        let id = get_string(station.get("Id"))?.map(str::trim).unwrap_or("");
        let name = get_string(station.get("Name"))?.map(str::trim).unwrap_or("");
        let tags = match station.get("Tags") {
            Some(Node::Array(tags)) => Some(tags),
            _ => None,
        };
        if id.is_empty() || !ids.insert(ordinal_ignore_case_key(id)) {
            return Ok(Some("Discovery station IDs must be present and unique".into()));
        }
        let name_length = utf16_len(name);
        if name_length == 0 || name_length > 100 || !names.insert(ordinal_ignore_case_key(name)) {
            return Ok(Some(
                "Discovery station names must be present, unique, and at most 100 characters".into(),
            ));
        }
        let bad_tags = match tags {
            None => true,
            Some(tags) if tags.is_empty() || tags.len() > 5 => true,
            Some(tags) => {
                let mut any_blank = false;
                for tag in tags {
                    if get_string(Some(tag))?.is_none_or(is_blank) {
                        any_blank = true;
                        break;
                    }
                }
                any_blank
            }
        };
        if bad_tags {
            return Ok(Some(format!(
                "{name} must contain between one and five non-empty tags"
            )));
        }
    }
    Ok(None)
}

// ---- secrets ---------------------------------------------------------------------------------

/// Swaps one placeholder back for the stored value, or drops the key when nothing is stored.
/// A real value is left alone.
fn restore_placeholder(incoming: Option<&mut JsonObject>, existing: Option<&JsonObject>, name: &str) {
    let Some(incoming) = incoming else { return };
    let Some(key) = key_of(incoming, name) else { return };
    if string_value(incoming.get(&key)) != Some(SECRET_PLACEHOLDER) {
        return;
    }
    let stored = existing
        .and_then(|existing| key_of(existing, name).and_then(|k| existing.get(&k)))
        .and_then(|node| string_value(Some(node)))
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    match stored {
        Some(text) => {
            incoming.insert(key, Node::String(text));
        }
        None => {
            incoming.shift_remove(&key);
        }
    }
}

/// Undo the placeholder in a Raw config document before it replaces the file: keep the stored
/// password when the file has one, otherwise drop the key so an environment value keeps
/// applying. A real value is left alone.
pub fn restore_secret_placeholders(incoming: &mut JsonObject, existing_file: &JsonObject) {
    restore_placeholder(
        child_mut(incoming, "Subsonic"),
        child(existing_file, "Subsonic"),
        "AdminPassword",
    );
    restore_placeholder(
        child_mut(incoming, "LastFm"),
        child(existing_file, "LastFm"),
        "ApiSecret",
    );

    // Each listener's session the same way, matched by username. An entry whose key is only in
    // the environment is dropped whole, so the environment keeps applying.
    let Some(sessions) = child_mut(incoming, "LastFm").and_then(|lastfm| child_mut(lastfm, "UserSessions"))
    else {
        return;
    };
    let stored_sessions = child(existing_file, "LastFm").and_then(|lastfm| child(lastfm, "UserSessions"));
    let users: Vec<String> = sessions.keys().cloned().collect();
    for user in users {
        let Some(Node::Object(session)) = sessions.get_mut(&user) else {
            continue;
        };
        let stored = stored_sessions
            .and_then(|stored| key_of(stored, &user).and_then(|k| stored.get(&k)))
            .and_then(Node::as_object);
        restore_placeholder(Some(session), stored, "SessionKey");
        if key_of(session, "SessionKey").is_none() {
            sessions.shift_remove(&user);
        }
    }
}

/// The listener whose Last.fm session key, after the placeholders were restored, still starts
/// with the placeholder: someone typed onto the end of it. Saved, it would be a key Last.fm
/// refuses, and the listener would be disconnected for it. None when there is none.
pub fn session_key_typed_into_placeholder(incoming: &JsonObject) -> Option<String> {
    let sessions = child(incoming, "LastFm").and_then(|lastfm| child(lastfm, "UserSessions"))?;
    sessions.iter().find_map(|(user, node)| {
        let session = node.as_object()?;
        let key = key_of(session, "SessionKey")?;
        string_value(session.get(&key))
            .filter(|text| text.starts_with(SECRET_PLACEHOLDER))
            .map(|_| user.clone())
    })
}

fn mask_in(section: Option<&mut JsonObject>, name: &str) {
    let Some(section) = section else { return };
    let keys: Vec<String> = section
        .keys()
        .filter(|key| octo_core::common::dotnet::eq_ignore_case(key, name))
        .cloned()
        .collect();
    for key in keys {
        if let Some(Node::String(text)) = section.get(&key) {
            let masked = mask_secret(Some(text));
            section.insert(key, Node::String(masked));
        }
    }
}

/// The merged file echoed after a save, with the admin password, the Last.fm secret and every
/// session key masked the same way the GET masks them.
pub fn redact_secrets(merged: &JsonObject) -> JsonObject {
    let mut copy = merged.clone();
    mask_in(child_mut(&mut copy, "Subsonic"), "AdminPassword");
    if let Some(lastfm) = child_mut(&mut copy, "LastFm") {
        mask_in(Some(lastfm), "ApiSecret");
        if let Some(sessions) = child_mut(lastfm, "UserSessions") {
            for (_, session) in sessions.iter_mut() {
                mask_in(session.as_object_mut(), "SessionKey");
            }
        }
    }
    copy
}

/// `POST /api/admin/settings`: writes a partial settings patch to the JSON file. The body
/// shape matches the GET; any subset of keys may be supplied. The file watcher picks the write
/// up within moments, so live settings reflect it on the next read, but services that captured
/// a value at startup keep theirs until a restart.
pub async fn save_settings(State(state): State<AppState>, body: Bytes) -> AppResult {
    let body = body_text(&body);
    if is_blank(&body) {
        return Ok(error_json(StatusCode::BAD_REQUEST, "empty body"));
    }
    let mut patch = match parse_json_node(&body) {
        Ok(Node::Object(map)) => map,
        Ok(_) => {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "invalid JSON: expected object",
            ));
        }
        Err(message) => {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {message}"),
            ));
        }
    };

    // Strip any meta-only keys the UI might echo back so they don't end up persisted to disk.
    patch.shift_remove("_meta");

    let current = state.settings.current();
    if let Some(Node::Object(actions_patch)) = patch.get("LibraryActions")
        && let Some(message) = validate_library_actions(actions_patch, &current.library_actions)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(Node::Object(genre_patch)) = patch.get("Genre")
        && let Some(Node::Array(mappings)) = genre_patch.get("Mappings")
        && let Some(message) = validate_genre_mappings(mappings)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(Node::Object(lastfm_patch)) = patch.get("LastFm")
        && let Some(Node::Array(stations)) = lastfm_patch.get("DiscoveryStations")
        && let Some(message) = validate_discovery_stations(stations)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }

    // The form echoes the placeholder back when the admin password was left alone; that means
    // "keep what is saved", so it must not be written. Anything typed after the placeholder
    // would otherwise be saved as the password. Keys are matched without regard to case, as
    // the configuration system matches them.
    if let Some(subsonic_patch) = child_mut(&mut patch, "Subsonic")
        && let Some(password_key) = key_of(subsonic_patch, "AdminPassword")
        && let Some(password) = string_value(subsonic_patch.get(&password_key))
    {
        if password.starts_with(SECRET_PLACEHOLDER) && password != SECRET_PLACEHOLDER {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "Retype the whole admin password; it was added to the hidden placeholder.",
            ));
        }
        if password == SECRET_PLACEHOLDER {
            // A different username with the old password kept is a pairing nobody asked for.
            let new_user = key_of(subsonic_patch, "AdminUsername")
                .and_then(|key| string_value(subsonic_patch.get(&key)).map(str::to_string));
            if let Some(new_user) = new_user
                && new_user.trim() != current.subsonic.admin_username.as_deref().unwrap_or("").trim()
            {
                return Ok(error_json(
                    StatusCode::BAD_REQUEST,
                    "Retype the admin password for the new username.",
                ));
            }
            subsonic_patch.shift_remove(&password_key);
        }
    }

    if let Some(lastfm_secrets) = child_mut(&mut patch, "LastFm") {
        // Sessions are made by Connect and removed by Disconnect. No form sends them, and an
        // echoed placeholder must never overwrite a real key.
        if let Some(sessions_key) = key_of(lastfm_secrets, "UserSessions") {
            lastfm_secrets.shift_remove(&sessions_key);
        }
        if let Some(secret_key) = key_of(lastfm_secrets, "ApiSecret")
            && let Some(secret) = string_value(lastfm_secrets.get(&secret_key))
            && secret.starts_with(SECRET_PLACEHOLDER)
        {
            if secret != SECRET_PLACEHOLDER {
                return Ok(error_json(
                    StatusCode::BAD_REQUEST,
                    "Retype the whole Last.fm shared secret; it was added to the hidden placeholder.",
                ));
            }
            lastfm_secrets.shift_remove(&secret_key);
        }
    }

    // UserTokens is a dictionary: merged, a user removed in the dashboard would stay.
    match state.settings_writer.merge(&patch, &["ListenBrainz.UserTokens"]) {
        Ok(merged) => {
            let keys: Vec<&str> = patch.keys().map(String::as_str).collect();
            info!("Admin settings updated: {}", keys.join(","));
            let mut answer = JsonObject::new();
            answer.insert("ok".into(), Node::Bool(true));
            answer.insert("persisted".into(), Node::Object(redact_secrets(&merged)));
            Ok(json_node(StatusCode::OK, &Node::Object(answer)))
        }
        Err(SettingsWriteError::Corrupt(e)) => {
            warn!("Refused to save settings: {} is not valid JSON", e.path.display());
            Ok(error_json(
                StatusCode::CONFLICT,
                &format!(
                    "{} Fix it in Raw config, or on disk at {}.",
                    e.message,
                    e.path.display()
                ),
            ))
        }
        Err(SettingsWriteError::Io(e)) => {
            error!(
                "Failed to persist settings to {}: {e}",
                state.settings_writer.file_path().display()
            );
            Ok(error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(json: &str) -> JsonObject {
        match Node::parse(json).expect("test JSON parses") {
            Node::Object(map) => map,
            _ => panic!("an object"),
        }
    }

    #[test]
    fn restore_secret_placeholders_keeps_the_stored_password() {
        let mut incoming = obj(&format!(
            r#"{{ "Subsonic": {{ "AdminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("stored")
        );
    }

    /// With nothing stored in the file the password came from the environment, so the key is
    /// dropped and the environment keeps applying.
    #[test]
    fn restore_secret_placeholders_drops_the_key_when_the_file_has_none() {
        let mut incoming = obj(&format!(
            r#"{{ "Subsonic": {{ "AdminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(&mut incoming, &obj("{}"));
        assert!(incoming["Subsonic"].get("AdminPassword").is_none());
    }

    #[test]
    fn restore_secret_placeholders_leaves_a_real_value_alone() {
        let mut incoming = obj(r#"{ "Subsonic": { "AdminPassword": "typed" } }"#);
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("typed")
        );
    }

    /// Configuration keys ignore case, so a hand-edited or scripted lowercase key must be
    /// handled exactly like the canonical one.
    #[test]
    fn placeholders_are_found_whatever_the_key_case() {
        let mut incoming = obj(&format!(
            r#"{{ "subsonic": {{ "adminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["subsonic"].get("adminPassword").and_then(Node::as_str),
            Some("stored")
        );

        let echoed = redact_secrets(&obj(r#"{ "subsonic": { "adminpassword": "stored" } }"#));
        assert_eq!(
            echoed["subsonic"].get("adminpassword").and_then(Node::as_str),
            Some(SECRET_PLACEHOLDER)
        );
    }

    #[test]
    fn redact_secrets_masks_the_admin_password_in_the_save_echo() {
        let merged = obj(r#"{ "Subsonic": { "AdminPassword": "stored", "Url": "http://x" } }"#);
        let echoed = redact_secrets(&merged);
        assert_eq!(
            echoed["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some(SECRET_PLACEHOLDER)
        );
        assert_eq!(
            merged["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("stored")
        );
    }

    /// Keep made five actions. A sixth is still a config that does not mean anything.
    #[test]
    fn validate_library_actions_accepts_five_refuses_six() {
        fn with_actions(count: usize) -> JsonObject {
            let actions = (0..count)
                .map(|i| n_obj([("Name", n_str(&format!("a{i}"))), ("Rating", n_num(0))]))
                .collect();
            let mut map = JsonObject::new();
            map.insert("Actions".into(), Node::Array(actions));
            map
        }
        let current = LibraryActionSettings::default();
        assert_eq!(
            validate_library_actions(&with_actions(5), &current).unwrap(),
            None
        );
        assert_eq!(
            validate_library_actions(&with_actions(6), &current)
                .unwrap()
                .as_deref(),
            Some("There are only five library actions")
        );
    }

    #[test]
    fn validators_throw_on_a_value_of_the_wrong_kind_as_get_value_did() {
        let current = LibraryActionSettings::default();
        let wrong = obj(r#"{ "Enabled": "true" }"#);
        assert!(matches!(
            validate_library_actions(&wrong, &current),
            Err(AppError::InvalidOperation(_))
        ));
        assert_eq!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("exact"))])])
                .unwrap(),
            None
        );
        assert_eq!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("7"))])]).unwrap(),
            None,
            "Enum.TryParse takes an undefined number"
        );
        assert!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("Fuzzy"))])])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_session_key_typed_onto_the_placeholder_names_its_listener() {
        let incoming = obj(&format!(
            r#"{{ "LastFm": {{ "UserSessions": {{ "alice": {{ "SessionKey": "{SECRET_PLACEHOLDER}x" }} }} }} }}"#
        ));
        assert_eq!(
            session_key_typed_into_placeholder(&incoming).as_deref(),
            Some("alice")
        );
        assert_eq!(session_key_typed_into_placeholder(&obj("{}")), None);
    }
}
