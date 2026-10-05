//! The cover.jpg beside an album, which Navidrome ranks above the art inside the files. Octo
//! marks every cover.jpg it writes (a JPEG comment), so it can later replace its own with a
//! larger one and never touch one the owner put there.

use std::path::Path;

use super::cover_image;
use super::text::eq_ignore_case;

pub const FILE_NAME: &str = "cover.jpg";

/// The names of the files in `dir` that `prefix*` matches. .NET matches search patterns
/// case-sensitively on Linux, as here.
fn files_like(dir: &Path, prefix: &str) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().starts_with(prefix) {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// Whether `cover` may go to `dir`/cover.jpg. Into a folder with no cover file only when the
/// caller made the folder (see the download path's sidecar rule). Over an existing one only
/// when it is Octo's own cover.jpg and smaller, and never when any other cover.* or folder.*
/// sits there.
pub fn should_write(dir: &Path, cover: &[u8], folder_is_new: bool) -> bool {
    let check = || -> std::io::Result<bool> {
        if !files_like(dir, "folder.")?.is_empty() {
            return Ok(false);
        }
        let existing = files_like(dir, "cover.")?;
        if existing.is_empty() {
            return Ok(folder_is_new);
        }
        let name = existing[0]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if existing.len() != 1 || !eq_ignore_case(&name, FILE_NAME) {
            return Ok(false);
        }
        let current = std::fs::read(&existing[0])?;
        Ok(cover_image::is_octo_cover(&current) && is_larger(cover, Some(&current)))
    };
    check().unwrap_or(false)
}

/// The longer side of `candidate` beats `current`'s (as C# documents it; the code, there and here, compares the
/// shorter sides).
pub fn is_larger(candidate: &[u8], current: Option<&[u8]>) -> bool {
    let Some(next) = cover_image::measure(candidate) else {
        return false;
    };
    match current.and_then(cover_image::measure) {
        None => true,
        Some(now) => next.0.min(next.1) > now.0.min(now.1),
    }
}

/// Writes the cover as a marked JPEG, through a temporary file, so a reader never sees half of it.
pub fn write(dir: &Path, cover: &[u8]) -> std::io::Result<()> {
    let path = dir.join(FILE_NAME);
    let temp = dir.join(format!("{FILE_NAME}.octo-tmp"));
    std::fs::write(&temp, cover_image::mark_as_octo(&cover_image::to_jpeg(cover)))?;
    // An owner's "Cover.JPG" on a case-sensitive disk is another file; should_write has
    // already refused that folder, so this only ever replaces Octo's own.
    std::fs::rename(&temp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(side: u32) -> Vec<u8> {
        cover_image::encode_jpeg(&image::DynamicImage::new_rgb8(side, side), 90).expect("encodes")
    }

    #[test]
    fn writes_only_over_its_own_smaller_cover() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(!should_write(dir.path(), &jpeg(300), false));
        assert!(should_write(dir.path(), &jpeg(300), true));
        write(dir.path(), &jpeg(300)).expect("writes");
        assert!(cover_image::is_octo_cover(
            &std::fs::read(dir.path().join(FILE_NAME)).expect("reads")
        ));
        assert!(!dir.path().join("cover.jpg.octo-tmp").exists());
        assert!(should_write(dir.path(), &jpeg(600), false));
        assert!(!should_write(dir.path(), &jpeg(200), false));

        std::fs::write(dir.path().join(FILE_NAME), jpeg(300)).expect("writes");
        assert!(
            !should_write(dir.path(), &jpeg(600), false),
            "the owner's own cover.jpg stays"
        );

        let other = tempfile::tempdir().expect("a temp dir");
        std::fs::write(other.path().join("folder.png"), b"x").expect("writes");
        assert!(!should_write(other.path(), &jpeg(600), true));
    }
}
