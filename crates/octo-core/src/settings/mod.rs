//! The settings classes (`Octo.Models.Settings`), the live store that binds them from the
//! layered configuration, and the admin helpers that edit `settings.json`
//! (`SettingsFileWriter`, `RestartTracker`).
//!
//! Each struct derives `Deserialize` with `#[serde(default)]` and carries the C# property
//! initializers in its `Default`; [`crate::config::bind`] fills it from a configuration
//! section. `Effective*` properties are methods that clamp at read time, as the C# did.

pub mod generated_playlist;
pub mod genre;
pub mod last_fm;
pub mod library_action;
pub mod lidarr;
pub mod listen_brainz;
pub mod metadata;
pub mod notification;
pub mod restart_tracker;
pub mod server;
pub mod soulseek;
pub mod store;
pub mod subsonic;
mod text;
pub mod update;
pub mod writer;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::{BindWarning, ConfigTree, bind};

pub use generated_playlist::GeneratedPlaylistSettings;
pub use genre::{
    GenreEmptyBehavior, GenreFallbackSource, GenreMappingSettings, GenreMatchMode, GenreSettings,
};
pub use last_fm::{DiscoveryStationSettings, LastFmSettings, LastFmUserSession};
pub use library_action::{
    LibraryAction, LibraryActionDefinition, LibraryActionSettings, LibraryActionTrigger, LibraryRatingScope,
    NoticeKind, UpgradeSourceChoice,
};
pub use lidarr::{LidarrCompletionMode, LidarrSettings};
pub use listen_brainz::ListenBrainzSettings;
pub use metadata::{LyricsSaveTo, MetadataSettings};
pub use notification::NotificationSettings;
pub use restart_tracker::RestartTracker;
pub use server::ServerSettings;
pub use soulseek::SoulseekSettings;
pub use store::{RawConfig, SettingsStore};
pub use subsonic::{
    DownloadMode, DownloadSource, ExplicitFilter, FolderStructure, HeartDownloadSource, HeartDownloadStep,
    StorageMode, SubsonicSettings,
};
pub use text::IgnoreCaseSet;
pub use update::UpdateSettings;
pub use writer::{JsonObject, SettingsFileCorruptError, SettingsFileWriter, SettingsWriteError};

/// Every bound settings section, as one snapshot. `Program.cs` registered each class with
/// `Configure<T>(Configuration.GetSection(name))`; the section names are the field names in
/// PascalCase.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AppSettings {
    pub subsonic: SubsonicSettings,
    pub soulseek: SoulseekSettings,
    pub lidarr: LidarrSettings,
    pub last_fm: LastFmSettings,
    pub genre: GenreSettings,
    pub library_actions: LibraryActionSettings,
    pub generated_playlists: GeneratedPlaylistSettings,
    pub notifications: NotificationSettings,
    pub metadata: MetadataSettings,
    pub server: ServerSettings,
    pub listen_brainz: ListenBrainzSettings,
    pub updates: UpdateSettings,
}

impl AppSettings {
    /// The configuration section names, in `Program.cs` registration order.
    pub const SECTIONS: [&'static str; 12] = [
        "Genre",
        "LibraryActions",
        "GeneratedPlaylists",
        "Subsonic",
        "Soulseek",
        "Lidarr",
        "LastFm",
        "Notifications",
        "Metadata",
        "Server",
        "ListenBrainz",
        "Updates",
    ];

    /// Binds every section from the merged configuration. A value that does not convert
    /// keeps its default (or a lower layer's value) and comes back as a warning whose path
    /// starts with the section name.
    pub fn bind(tree: &ConfigTree) -> (AppSettings, Vec<BindWarning>) {
        let mut warnings = Vec::new();
        let settings = AppSettings {
            genre: bind_section(tree, "Genre", &mut warnings),
            library_actions: bind_section(tree, "LibraryActions", &mut warnings),
            generated_playlists: bind_section(tree, "GeneratedPlaylists", &mut warnings),
            subsonic: bind_section(tree, "Subsonic", &mut warnings),
            soulseek: bind_section(tree, "Soulseek", &mut warnings),
            lidarr: bind_section(tree, "Lidarr", &mut warnings),
            last_fm: bind_section(tree, "LastFm", &mut warnings),
            notifications: bind_section(tree, "Notifications", &mut warnings),
            metadata: bind_section(tree, "Metadata", &mut warnings),
            server: bind_section(tree, "Server", &mut warnings),
            listen_brainz: bind_section(tree, "ListenBrainz", &mut warnings),
            updates: bind_section(tree, "Updates", &mut warnings),
        };
        (settings, warnings)
    }
}

/// `Configuration.GetSection(name).Get<T>()`, collecting conversion warnings under the
/// section's path. AdminController read `Updates` this way on every request.
pub fn bind_section<T: DeserializeOwned + Default>(
    tree: &ConfigTree,
    name: &str,
    warnings: &mut Vec<BindWarning>,
) -> T {
    let (value, w) = bind::<T>(&tree.section(name));
    warnings.extend(w.into_iter().map(|w| BindWarning {
        path: if w.path.is_empty() {
            name.to_string()
        } else {
            format!("{name}:{}", w.path)
        },
        message: w.message,
    }));
    value
}
