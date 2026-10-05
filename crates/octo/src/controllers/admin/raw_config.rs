//! `AdminController`'s Raw config editor and the config-sources table (L1643–L2086):
//! `GET`/`PUT /api/admin/raw-config` and `GET /api/admin/config-sources`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::common::dotnet::{is_blank, utf16_len};
use octo_core::json::dom::Node;
use octo_core::settings::JsonObject;
use octo_core::settings::writer::to_indented_json;
use serde_json::json;
use tracing::{error, info};

use super::helpers_6b1::{
    SECRET_PLACEHOLDER, body_text, child, error_json, json_text, key_of, n_obj, parse_json_node,
};
use super::settings::{
    effective_document, restore_secret_placeholders, session_key_typed_into_placeholder, string_value,
};
use crate::app::AppState;
use crate::http::error::json_ok;

/// `GET /api/admin/raw-config`: the *effective* configuration as a JSON document, with values
/// from the live settings (what the app actually sees). Anything persisted to settings.json
/// sits on top of env vars and appsettings.json defaults; that merged result is what goes out,
/// so the Raw Config editor shows the full picture rather than only the sparse overrides file.
///
/// On PUT the entire body is written wholesale to settings.json, which is a no-op if the user
/// just hits Save without editing (values match env) and a real override otherwise.
pub async fn get_raw_config(State(state): State<AppState>) -> Response {
    let document = effective_document(&state, true);
    // Content(json, "application/json"): no charset.
    json_text(StatusCode::OK, to_indented_json(&document), "application/json")
}

/// The secret's value when it starts with the placeholder: someone typed onto its end.
fn typed_onto_placeholder(section: Option<&JsonObject>, name: &str) -> bool {
    section
        .and_then(|section| key_of(section, name).and_then(|key| section.get(&key)))
        .and_then(|node| string_value(Some(node)))
        .is_some_and(|text| text.starts_with(SECRET_PLACEHOLDER))
}

/// `PUT /api/admin/raw-config`: replaces settings.json wholesale with the request body, after
/// checking that it parses as a JSON object, so the user cannot save a broken file that crashes
/// the next start.
pub async fn put_raw_config(State(state): State<AppState>, body: Bytes) -> Response {
    let body = body_text(&body);
    if is_blank(&body) {
        return error_json(StatusCode::BAD_REQUEST, "empty body");
    }
    let mut parsed = match parse_json_node(&body) {
        Ok(Node::Object(map)) => map,
        Ok(_) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid JSON: top level must be an object",
            );
        }
        Err(message) => return error_json(StatusCode::BAD_REQUEST, &format!("invalid JSON: {message}")),
    };

    // Don't merge: this is the "I know exactly what I want" power-user endpoint. It goes
    // through the writer so it shares the lock and atomic write with form saves, and the hidden
    // admin password comes back as a placeholder that must not be saved literally. When the
    // file cannot be read, the running value is the only copy of the password left, and this
    // save is the recovery path the 409 points people to.
    let existing = if state.settings_writer.is_readable() {
        state.settings_writer.load()
    } else {
        let current = state.settings.current();
        let password = current
            .subsonic
            .admin_password
            .as_deref()
            .map_or(Node::Null, |p| Node::String(p.to_string()));
        let sessions = Node::Object(
            current
                .last_fm
                .user_sessions
                .iter()
                .map(|(user, session)| {
                    (
                        user.clone(),
                        n_obj([
                            ("SessionKey", Node::String(session.session_key.clone())),
                            ("LastFmUser", Node::String(session.last_fm_user.clone())),
                        ]),
                    )
                })
                .collect(),
        );
        let mut existing = JsonObject::new();
        existing.insert("Subsonic".into(), n_obj([("AdminPassword", password)]));
        existing.insert(
            "LastFm".into(),
            n_obj([
                ("ApiSecret", Node::String(current.last_fm.api_secret.clone())),
                ("UserSessions", sessions),
            ]),
        );
        existing
    };
    restore_secret_placeholders(&mut parsed, &existing);
    if typed_onto_placeholder(child(&parsed, "Subsonic"), "AdminPassword") {
        return error_json(
            StatusCode::BAD_REQUEST,
            "Retype the whole admin password; it was added to the hidden placeholder.",
        );
    }
    if typed_onto_placeholder(child(&parsed, "LastFm"), "ApiSecret") {
        return error_json(
            StatusCode::BAD_REQUEST,
            "Retype the whole Last.fm shared secret; it was added to the hidden placeholder.",
        );
    }
    if let Some(user) = session_key_typed_into_placeholder(&parsed) {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!(
                "Connect {user} to Last.fm again, or paste their whole session key; it was added to the hidden placeholder."
            ),
        );
    }
    let pretty = to_indented_json(&parsed);
    match state.settings_writer.replace(&parsed) {
        Ok(()) => {
            let bytes = utf16_len(&pretty);
            info!("Admin raw-config saved ({bytes} bytes)");
            json_ok(&json!({ "ok": true, "bytes": bytes }))
        }
        Err(e) => {
            error!(
                "Failed to write raw config to {}: {e}",
                state.settings_writer.file_path().display()
            );
            error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// The keys `GET config-sources` lists, in the dashboard's order.
const CONFIG_SOURCE_KEYS: &[&str] = &[
    "Subsonic:Url",
    "Subsonic:StorageMode",
    "Subsonic:DownloadMode",
    "Subsonic:DownloadOnStar",
    "Subsonic:DownloadAlbumOnStar",
    "Subsonic:RecordRequestedBy",
    "Subsonic:StarDownloadsForRequester",
    "Subsonic:WaitForLosslessOnPlay",
    "Subsonic:LosslessWaitTimeoutSeconds",
    "Subsonic:DownloadOnPlay",
    "Subsonic:LidarrAlbumOnPlay",
    "Subsonic:DownloadSource",
    "Subsonic:AutoDetectDownloadPath",
    "Subsonic:LibraryPath",
    "Subsonic:FolderStructure",
    "Subsonic:UseLocalStaging",
    "Subsonic:ExplicitFilter",
    "Subsonic:CacheDurationHours",
    "Subsonic:EnableExternalPlaylists",
    "Subsonic:WaitForSearchDurations",
    "Subsonic:SkipOwnedSongs",
    "Subsonic:PlaylistsDirectory",
    "Library:DownloadPath",
    "Server:PublicUrl",
    "Updates:Check",
    "Updates:Repo",
    "Soulseek:BaseUrl",
    "Soulseek:Username",
    "Soulseek:Password",
    "Soulseek:SearchWaitSeconds",
    "Soulseek:UpgradeSearchWaitSeconds",
    "Soulseek:MinFileSizeBytes",
    "Soulseek:PreferredExtension",
    "Soulseek:DownloadTimeoutSeconds",
    "Soulseek:RejectedPeerTtlDays",
    "Soulseek:FingerprintSeconds",
    "Soulseek:FingerprintTimeoutSeconds",
    "Soulseek:AcoustIdTimeoutSeconds",
    "Soulseek:DetectTranscodes",
    "Soulseek:TranscodeCheckTimeoutSeconds",
    "Soulseek:OutageHoldHours",
    "Soulseek:ParallelDownloads",
    "Soulseek:AlbumFolders",
    "Genre:BackfillMaxConsecutiveFailures",
    "Genre:BackfillExtensions",
    "LibraryActions:Enabled",
    "LibraryActions:PlaylistsEnabled",
    "LibraryActions:RatingsEnabled",
    "LibraryActions:PlaylistPrefix",
    "LibraryActions:DryRun",
    "LibraryActions:QuarantineDirectory",
    "LibraryActions:QuarantineRetentionDays",
    "LibraryActions:PollIntervalSeconds",
    "LibraryActions:MaxActionsPerCycle",
    "LibraryActions:KeepReplacedOriginals",
    "LibraryActions:Actions",
    "LibraryActions:AllowedUsers",
    "LibraryActions:NoticePrefix",
    "LibraryActions:ReviewEnabled",
    "LibraryActions:ReviewPlaylistName",
    "LibraryActions:NoticeMaxTracks",
    "LibraryActions:RatingsScope",
    "LibraryActions:DuplicatesEnabled",
    "LibraryActions:DuplicatesPlaylistName",
    "LibraryActions:DuplicatesScanHours",
    "LibraryActions:UpgradePerWeek",
    "LibraryActions:UpgradeSource",
    "LibraryActions:ReviewSweepPerHour",
    "LibraryActions:ReviewSweepOctoDownloads",
    "Soulseek:SubmitConfirmedFingerprints",
    "Soulseek:AcoustIdUserApiKey",
    "Genre:Enabled",
    "Genre:MaxGenres",
    "Genre:OnEmpty",
    "Genre:Fallback",
    "Genre:UnknownLabel",
    "Genre:Mappings",
    "Genre:Blocklist",
    "Soulseek:VerifyDownloads",
    "Soulseek:AcoustIdApiKey",
    "Soulseek:MinMatchScore",
    "Soulseek:TagFromMusicBrainz",
    "Soulseek:NameFromMatch",
    "Lidarr:BaseUrl",
    "Lidarr:ApiKey",
    "Lidarr:RootFolderPath",
    "Lidarr:QualityProfileId",
    "Lidarr:MetadataProfileId",
    "Lidarr:CompletionMode",
    "Lidarr:ImportTimeoutSeconds",
    "YouTube:ShimUrl",
    "LastFm:ApiKey",
    "LastFm:ApiSecret",
    "LastFm:ScrobbleExternalPlays",
    "LastFm:ScrobbleLibraryPlays",
    "LastFm:EnableRadio",
    "LastFm:RadioTrackCount",
    "LastFm:RadioCacheDurationHours",
    "LastFm:StarterPublishTimeoutSeconds",
    "LastFm:RadioLoudnessTargetLufs",
    "LastFm:EnablePersonalizedStations",
    "LastFm:EnableYourMix",
    "LastFm:EnableDiscoveryMix",
    "LastFm:ArtistStationCount",
    "LastFm:GenreStationCount",
    "LastFm:EnableDiscoveryStations",
    "LastFm:HistoryRetentionDays",
    "LastFm:DiscoveryPercent",
    "LastFm:RefreshIntervalHours",
    "LastFm:MinimumPlays",
    "LastFm:DiscoveryStations",
    "Metadata:Language",
    "Metadata:AlbumFromTitle",
    "Metadata:UseCoverArtArchive",
    "Metadata:ReplaceVideoCovers",
    "Metadata:WriteCoverFile",
    "Metadata:EmbedFullSizeCovers",
    "Metadata:FetchLyrics",
    "Metadata:LyricsSources",
    "Metadata:PreferWordTimedLyrics",
    "Metadata:WriteLyricsBesideAllSongs",
    "Metadata:SaveLyricsTo",
    "Metadata:PreferOriginalAlbum",
    "Metadata:YearFromOriginalRelease",
    "Metadata:PreferredCountries",
    "Metadata:ReleaseDetailsLookup",
    "Metadata:ReplayGain",
    "Metadata:ReplayGainTimeoutSeconds",
    "Metadata:TagRehearsal",
    "GeneratedPlaylists:Enabled",
    "GeneratedPlaylists:Genres",
    "GeneratedPlaylists:Decades",
    "GeneratedPlaylists:TrackCount",
    "GeneratedPlaylists:MaxPerArtist",
    "GeneratedPlaylists:CreateAt",
    "GeneratedPlaylists:RemoveBelow",
    "GeneratedPlaylists:MaxPlaylists",
    "GeneratedPlaylists:RefreshHours",
    "GeneratedPlaylists:NewShare",
    "GeneratedPlaylists:NewDays",
    "GeneratedPlaylists:NameFormat",
    "Notifications:NtfyUrl",
    "Notifications:NtfyToken",
    "Notifications:DiscordWebhookUrl",
    "Notifications:NotifyDownloadStarted",
    "Notifications:NotifyDownloadCompleted",
    "Notifications:NotifyLosslessFallback",
    "Notifications:NotifyDownloadFailed",
    "Notifications:NotifyAlbumCompleted",
    "ListenBrainz:Token",
    "ListenBrainz:SubmitExternalPlays",
];

/// Whether a config-sources key names a secret: a password, an API key, a secret, a token
/// (ntfy tokens are credentials outright) or a webhook URL (a Discord webhook URL embeds its
/// token, so the whole URL is the secret).
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["password", "apikey", "secret", "token", "webhookurl"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// `GET /api/admin/config-sources`: every config key the app effectively sees, as
/// `IConfiguration[key]` reads it (a list reads as `""`, having no value of its own), with
/// anything that smells like a secret masked so a screenshot of the page does not leak it.
pub async fn get_config_sources(State(state): State<AppState>) -> Response {
    let rows: Vec<serde_json::Value> = CONFIG_SOURCE_KEYS
        .iter()
        .map(|key| {
            let value = state.settings.raw(key).unwrap_or_default();
            let secret = is_secret_key(key);
            let display = if secret && !value.is_empty() {
                "\u{2022}".repeat(utf16_len(&value).min(16))
            } else {
                value
            };
            json!({ "Key": key, "Value": display, "IsSecret": secret })
        })
        .collect();
    json_ok(&json!({
        "keys": rows,
        "configFile": state.settings_writer.file_path().display().to_string(),
    }))
}
