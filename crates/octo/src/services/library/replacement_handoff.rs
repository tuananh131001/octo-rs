//! Port of `Services/Library/ReplacementHandoff.cs`.

use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use octo_media::tags::KeptIdentity;
use parking_lot::Mutex;

/// Called once the replacement is tagged and still out of the scanner's sight, with its path.
/// `None` means go ahead, and the original has been moved out; otherwise why it is refused.
pub type BeforeReveal = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Called with the new path right after the replacement moved in.
pub type OnRevealed = Arc<dyn Fn(&str) + Send + Sync>;

/// What a library action hands the download so the replacement takes the original's place in
/// Navidrome rather than arriving as a new song (W8).
pub struct ReplacementHandoff {
    pub original_path: String,
    pub identity: KeptIdentity,

    /// Called once the replacement is tagged and still out of the scanner's sight.
    /// `None` means go ahead, and the original has been moved out; otherwise why it is refused.
    pub before_reveal: BeforeReveal,

    /// Set by the download once the replacement moved in. `None` when the download ran
    /// without this handoff, because it joined one already in flight.
    revealed_path: Mutex<Option<String>>,

    /// Called with the new path right after the replacement moved in, so the library
    /// action can record the swap before anything else that could fail runs.
    pub on_revealed: Option<OnRevealed>,
}

impl ReplacementHandoff {
    pub fn new(
        original_path: impl Into<String>,
        identity: KeptIdentity,
        before_reveal: BeforeReveal,
        on_revealed: Option<OnRevealed>,
    ) -> Self {
        ReplacementHandoff {
            original_path: original_path.into(),
            identity,
            before_reveal,
            revealed_path: Mutex::new(None),
            on_revealed,
        }
    }

    pub fn revealed_path(&self) -> Option<String> {
        self.revealed_path.lock().clone()
    }

    /// `RevealedPath { internal set; }`: the download records where the replacement moved in.
    pub fn set_revealed_path(&self, path: impl Into<String>) {
        *self.revealed_path.lock() = Some(path.into());
    }

    /// The original's folder and name with the new extension, or `None` when another
    /// file holds that name. With the same extension this is the original's own path, which
    /// Navidrome updates in place.
    pub fn target_for(&self, extension: &str) -> Option<String> {
        let original = Path::new(&self.original_path);
        let directory = original
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = original
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Path.GetFileNameWithoutExtension: everything before the last dot of the name.
        let stem = match name.rfind('.') {
            Some(dot) => &name[..dot],
            None => name.as_str(),
        };
        let file = format!("{stem}{extension}");
        let target = if directory.is_empty() {
            file
        } else {
            Path::new(&directory).join(file).to_string_lossy().into_owned()
        };
        if std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
            None
        } else {
            Some(target)
        }
    }
}

/// The library action refused the replacement before it was ever in the library.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{problem}")]
pub struct ReplacementRejectedException {
    pub problem: String,
}

impl ReplacementRejectedException {
    pub fn new(problem: impl Into<String>) -> Self {
        ReplacementRejectedException {
            problem: problem.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> KeptIdentity {
        KeptIdentity {
            title: "Song".into(),
            album: None,
            album_artist: Vec::new(),
            album_artists: Vec::new(),
            album_version: None,
            release_date: None,
            album_id: None,
            release_track_id: None,
            track: 0,
            track_count: 0,
            disc: 0,
            disc_count: 0,
            compilation: false,
        }
    }

    /// Rust-only: the target keeps the original's folder and name, takes the new extension, and
    /// is refused when another file holds it (the original's own path is the original itself).
    #[test]
    fn the_target_is_the_originals_place_with_the_new_extension() {
        let dir = tempfile::tempdir().expect("temp dir");
        let original = dir.path().join("01 Song.mp3");
        std::fs::write(&original, [1]).expect("write");
        let handoff = ReplacementHandoff::new(
            original.to_string_lossy(),
            identity(),
            Arc::new(|_| Box::pin(async { None })),
            None,
        );

        assert_eq!(
            handoff.target_for(".flac").as_deref(),
            Some(dir.path().join("01 Song.flac").to_string_lossy().as_ref())
        );
        assert_eq!(handoff.target_for(".mp3"), None);
        assert_eq!(handoff.revealed_path(), None);
        handoff.set_revealed_path("/music/01 Song.flac");
        assert_eq!(handoff.revealed_path().as_deref(), Some("/music/01 Song.flac"));
        assert_eq!(ReplacementRejectedException::new("no").to_string(), "no");
    }
}
