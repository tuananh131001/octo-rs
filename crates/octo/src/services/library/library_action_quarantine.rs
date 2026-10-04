//! Port of `Services/Library/LibraryActionQuarantine.cs`, and the `<file>.octo-action.json`
//! manifest beside each quarantined file (state-files.md §4.24).

use std::io;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use octo_core::json::datetime;
use octo_core::settings::{LibraryAction, SettingsStore};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::ResolvedSongFile;
use super::library_action_journal::action_name;
use super::navidrome_song_path_resolver::get_full_path;
use crate::services::state_file;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineResult {
    pub moved: bool,
    pub quarantine_path: Option<String>,
    pub error: Option<String>,
}

impl QuarantineResult {
    fn moved(path: String) -> Self {
        QuarantineResult {
            moved: true,
            quarantine_path: Some(path),
            error: None,
        }
    }

    fn refused(error: impl Into<String>) -> Self {
        QuarantineResult {
            moved: false,
            quarantine_path: None,
            error: Some(error.into()),
        }
    }
}

/// What is written next to a quarantined file so a restore works without the journal. Its
/// `Action` is the enum's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuarantineManifest {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub original_path: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub navidrome_id: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub action: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub username: String,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
}

/// Moves a verified file out of the library instead of deleting it.
///
/// There is no File.Delete anywhere in library actions except the retention sweep. These
/// actions delete files Octo did NOT create, on one tap in a music client with no confirmation
/// dialog, and the whole point of the feature is that the user is correcting a mistake, which
/// means they can make one.
///
/// This is where DiscardRejectedDownload's philosophy is reconciled rather than contradicted.
/// That guard refuses to delete anything created before the attempt started, because its input
/// is a GUESS: ResolveLocalPath matches on leaf name and approximate size. Here the user has
/// pointed at a specific track, so a creation-time guard would break the feature's purpose. The
/// invariant underneath still holds: never act on a path you inferred rather than proved. That
/// is discharged by the resolver's byte-size check, which is strictly stronger than
/// DiscardRejectedDownload's own approximate-size matching, plus quarantine instead of delete,
/// which makes getting it wrong recoverable rather than terminal.
pub struct LibraryActionQuarantine {
    /// `IOptionsMonitor<LibraryActionSettings>`: the folder and the retention are read at use.
    settings: Arc<SettingsStore>,
}

impl LibraryActionQuarantine {
    pub const MANIFEST_SUFFIX: &'static str = ".octo-action.json";

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        LibraryActionQuarantine { settings }
    }

    pub fn root_for(&self, music_root: &str) -> String {
        combine(
            music_root,
            &self
                .settings
                .current()
                .library_actions
                .effective_quarantine_directory(),
        )
    }

    /// Move a verified file into quarantine, preserving its layout underneath so a restore is a
    /// straight copy back. (C# `Move`, a keyword here.)
    pub fn move_file(
        &self,
        file: &ResolvedSongFile,
        music_root: &str,
        action: LibraryAction,
        username: &str,
    ) -> QuarantineResult {
        match self.try_move(file, music_root, action, username) {
            Ok(result) => result,
            Err(e) => {
                warn!("Library action could not quarantine {}: {e}", file.absolute_path);
                QuarantineResult::refused(e.to_string())
            }
        }
    }

    fn try_move(
        &self,
        file: &ResolvedSongFile,
        music_root: &str,
        action: LibraryAction,
        username: &str,
    ) -> io::Result<QuarantineResult> {
        let relative = get_relative_path(&get_full_path(music_root), &get_full_path(&file.absolute_path));
        if relative.starts_with("..") {
            return Ok(QuarantineResult::refused("the file is not under the music root"));
        }

        let destination = combine(
            &combine(
                &self.root_for(music_root),
                &Utc::now().format("%Y-%m-%d").to_string(),
            ),
            &relative,
        );
        if let Some(parent) = Path::new(&destination).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let destination = unique(&destination);

        if let Err(moving) = rename_new(&file.absolute_path, &destination) {
            // A refused permission was UnauthorizedAccessException, not an IOException, so it
            // never reached the copy.
            if moving.kind() == io::ErrorKind::PermissionDenied {
                return Err(moving);
            }
            // Across devices Move fails, so copy, confirm the length, then remove. The
            // length check is what stops a truncated copy from turning into a delete.
            copy_new(&file.absolute_path, &destination)?;
            if std::fs::metadata(&destination)?.len() as i64 != file.size_bytes {
                try_delete(&destination);
                return Ok(QuarantineResult::refused(
                    "the copy did not match the original's size",
                ));
            }
            std::fs::remove_file(&file.absolute_path)?;
        }

        self.write_manifest(
            &destination,
            &QuarantineManifest {
                original_path: file.absolute_path.clone(),
                navidrome_id: file.navidrome_id.clone(),
                action: action_name(action).to_string(),
                username: username.to_string(),
                at_utc: Utc::now(),
            },
        );

        info!(
            "Library action {} by {username}: quarantined {} -> {destination}",
            action_name(action),
            file.absolute_path
        );
        Ok(QuarantineResult::moved(destination))
    }

    /// Put a quarantined file back where it came from.
    pub fn restore(&self, quarantine_path: &str) -> QuarantineResult {
        let attempt = || -> io::Result<QuarantineResult> {
            if !file_exists(quarantine_path) {
                return Ok(QuarantineResult::refused("the quarantined file is gone"));
            }

            let Some(manifest) = self.read_manifest(quarantine_path) else {
                return Ok(QuarantineResult::refused(
                    "no manifest, so the original path is unknown",
                ));
            };

            if let Some(parent) = Path::new(&manifest.original_path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            if file_exists(&manifest.original_path) {
                return Ok(QuarantineResult::refused(
                    "something already exists at the original path",
                ));
            }

            rename_new(quarantine_path, &manifest.original_path)?;
            try_delete(&format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX));

            info!("Library action restored {}", manifest.original_path);
            Ok(QuarantineResult::moved(manifest.original_path))
        };
        attempt().unwrap_or_else(|e| {
            warn!("Library action could not restore {quarantine_path}: {e}");
            QuarantineResult::refused(e.to_string())
        })
    }

    /// The only code in this feature that really deletes, and it goes on age rather than on
    /// which action produced the file. Retention 0 means never sweep.
    pub fn sweep(&self, music_root: &str) -> i32 {
        let days = self
            .settings
            .current()
            .library_actions
            .effective_quarantine_retention_days();
        if days <= 0 {
            return 0;
        }

        let root = self.root_for(music_root);
        if !Path::new(&root).is_dir() {
            return 0;
        }

        let cutoff = Utc::now() - TimeDelta::days(i64::from(days));
        let mut removed = 0;
        let mut attempt = || -> io::Result<()> {
            for dated in std::fs::read_dir(&root)? {
                let dated = dated?;
                if !dated.file_type()?.is_dir() {
                    continue;
                }
                let name = dated.file_name().to_string_lossy().into_owned();
                let Some(day) = parse_day(&name) else {
                    continue;
                };
                if day >= cutoff {
                    continue;
                }

                let count = count_files(&dated.path())?;
                std::fs::remove_dir_all(dated.path())?;
                removed += count;
                info!("Library action quarantine swept {name} ({count} file(s))");
            }
            Ok(())
        };
        if let Err(e) = attempt() {
            warn!("Library action quarantine sweep failed: {e}");
        }
        removed
    }

    /// Written straight over the path, not through a temporary file, as the C# did.
    fn write_manifest(&self, quarantine_path: &str, manifest: &QuarantineManifest) {
        let path = format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX);
        if let Err(e) = std::fs::write(&path, octo_core::json::to_string(manifest)) {
            // The journal still records this, so a missing manifest costs the standalone
            // restore rather than the recovery entirely.
            warn!("Library action could not write a quarantine manifest: {e}");
        }
    }

    pub fn read_manifest(&self, quarantine_path: &str) -> Option<QuarantineManifest> {
        let path = format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX);
        let text = state_file::read_text(Path::new(&path)).ok()??;
        serde_json::from_str(&text).ok()
    }
}

/// `File.Move(from, to)` without overwrite: refused when something is already at `to`.
fn rename_new(from: &str, to: &str) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Cannot create a file when that file already exists: '{to}'"),
        ));
    }
    std::fs::rename(from, to)
}

/// `File.Copy(from, to, overwrite: false)`.
fn copy_new(from: &str, to: &str) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("The file '{to}' already exists."),
        ));
    }
    std::fs::copy(from, to).map(|_| ())
}

/// A free name for `path`: itself, then ` (2)` to ` (999)` before the extension, then a GUID.
fn unique(path: &str) -> String {
    if !file_exists(path) {
        return path.to_string();
    }
    let path_ref = Path::new(path);
    let directory = path_ref
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = path_ref
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Path.GetFileNameWithoutExtension / GetExtension: split at the name's last dot.
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => (name[..dot].to_string(), name[dot..].to_string()),
        Some(dot) => (name[..dot].to_string(), String::new()),
        None => (name.clone(), String::new()),
    };
    for suffix in 2..1000 {
        let candidate = combine(&directory, &format!("{stem} ({suffix}){extension}"));
        if !file_exists(&candidate) {
            return candidate;
        }
    }
    combine(
        &directory,
        &format!("{stem} ({}){extension}", uuid::Uuid::new_v4().simple()),
    )
}

/// `DateTime.TryParseExact(name, "yyyy-MM-dd", AssumeUniversal | AdjustToUniversal)`.
fn parse_day(name: &str) -> Option<DateTime<Utc>> {
    let bytes = name.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if !shaped {
        return None;
    }
    NaiveDate::parse_from_str(name, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

/// The files under a dated folder, not counting the manifests.
fn count_files(dir: &Path) -> io::Result<i32> {
    let mut count = 0;
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_dir() {
            count += count_files(&item.path())?;
        } else if !item
            .file_name()
            .to_string_lossy()
            .ends_with(LibraryActionQuarantine::MANIFEST_SUFFIX)
        {
            count += 1;
        }
    }
    Ok(count)
}

/// `Path.Combine(a, b)` on Unix: `b` when it is rooted, otherwise joined with one separator.
pub(crate) fn combine(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `Path.GetRelativePath(relativeTo, path)` for two full paths on Unix (case-sensitive): "."
/// for the same path, the rest of `path` under `relative_to`, otherwise a climb out with `..`.
pub(crate) fn get_relative_path(relative_to: &str, path: &str) -> String {
    let from: Vec<&str> = relative_to.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == from.len() && common == to.len() {
        return ".".to_string();
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    let mut relative = parts.join("/");
    if path.ends_with('/') && !relative.is_empty() {
        relative.push('/');
    }
    relative
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn try_delete(path: &str) {
    if file_exists(path) {
        // Best effort.
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
#[path = "library_action_quarantine_tests.rs"]
mod tests;
