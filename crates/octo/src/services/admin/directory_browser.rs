//! Port of `Services/Admin/DirectoryBrowser.cs`.

use std::path::Path;

use octo_core::common::dotnet::{compare_ordinal_ignore_case, eq_ignore_case};
use serde::Serialize;
use tracing::debug;

/// One directory offered to the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub writable: bool,
}

/// A directory listing: where we are, where up is, and what is here.
/// `audio_files` counts audio files directly in this folder. It exists because listing
/// directories alone makes a flat library — thousands of loose tracks and a handful of album
/// folders — look almost empty, giving no way to tell the right folder from a stray one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub separator: String,
    pub writable: bool,
    pub exists: bool,
    pub entries: Vec<BrowseEntry>,
    pub truncated: bool,
    pub audio_files: i32,
}

/// Lists directories so the admin UI can pick a download folder instead of the user
/// typing one from memory and discovering later that downloads landed somewhere
/// Navidrome never scans.
///
/// This browses Octo's OWN view of the filesystem, which under Docker is the
/// container's mount namespace, not the host's drives. That is the useful view: what
/// matters is where Octo can write, not where the host thinks the files are.
///
/// Directories only, never file names. Choosing a library folder does not need them,
/// and leaving them out keeps the amount this endpoint can disclose to the minimum
/// the feature actually requires.
///
/// Unix paths only: the C# offered the drives on Windows, which Octo never runs on.
#[derive(Debug, Default)]
pub struct DirectoryBrowser;

/// Extensions counted as music. Only the count is ever reported, never the
/// file names, so the picker can say "2,352 tracks here" without turning into
/// a file lister.
const AUDIO_EXTENSIONS: [&str; 16] = [
    ".flac", ".mp3", ".m4a", ".aac", ".ogg", ".oga", ".opus", ".wav", ".wma", ".aiff", ".aif", ".ape", ".wv",
    ".mpc", ".dsf", ".dff",
];

const SEPARATOR: &str = "/";

impl DirectoryBrowser {
    /// A pathological directory must not hang the UI or the response.
    pub const MAX_ENTRIES: usize = 1000;

    pub fn new() -> Self {
        DirectoryBrowser
    }

    pub fn browse(&self, path: Option<&str>) -> BrowseResult {
        // No path means "start somewhere sensible": the root.
        let Some(path) = path.filter(|p| !p.trim().is_empty()) else {
            return self.listing("/");
        };

        // Canonicalise before anything else so what we list is exactly what we
        // report back, and a caller cannot describe one directory and be shown
        // another via `..` segments.
        match get_full_path(path.trim()) {
            Some(full) => self.listing(&full),
            None => {
                debug!("browse: rejected path {path}: Null character in path.");
                BrowseResult {
                    path: path.to_string(),
                    parent: None,
                    separator: SEPARATOR.to_string(),
                    writable: false,
                    exists: false,
                    entries: Vec::new(),
                    truncated: false,
                    audio_files: 0,
                }
            }
        }
    }

    fn listing(&self, full: &str) -> BrowseResult {
        let result = |writable, exists, entries, truncated, audio_files| BrowseResult {
            path: full.to_string(),
            parent: parent_of(full),
            separator: SEPARATOR.to_string(),
            writable,
            exists,
            entries,
            truncated,
            audio_files,
        };
        if !Path::new(full).is_dir() {
            return result(false, false, Vec::new(), false, 0);
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let mut audio_files = 0;
        // One pass over the directory yields both the subfolders and the audio
        // count. Counting is free here because the enumeration is already
        // happening; doing it per subfolder instead would cost a round trip
        // each, and on a cloud mount that is seconds per folder.
        let listed = (|| -> std::io::Result<()> {
            for item in std::fs::read_dir(full)? {
                let name = item?.file_name().to_string_lossy().into_owned();
                let item = join(full, &name);
                if Path::new(&item).is_dir() {
                    if entries.len() >= Self::MAX_ENTRIES {
                        truncated = true;
                        continue;
                    }
                    let writable = is_writable(&item);
                    entries.push(BrowseEntry {
                        name,
                        path: item,
                        writable,
                    });
                } else if AUDIO_EXTENSIONS
                    .iter()
                    .any(|ext| eq_ignore_case(ext, get_extension(&item)))
                {
                    audio_files += 1;
                }
            }
            Ok(())
        })();
        if let Err(e) = listed {
            debug!("browse: cannot enumerate {full}: {e}");
            return result(false, true, Vec::new(), false, 0);
        }

        entries.sort_by(|a, b| compare_ordinal_ignore_case(&a.name, &b.name));
        result(is_writable(full), true, entries, truncated, audio_files)
    }
}

/// `Path.GetFullPath` on Unix: a relative path is taken from the working directory; `.`,
/// `..` (never above the root) and repeated separators are removed; a trailing separator is
/// kept. None for a path holding a NUL, which .NET refused with an ArgumentException.
fn get_full_path(path: &str) -> Option<String> {
    if path.contains('\0') {
        return None;
    }
    let combined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir().ok()?.to_string_lossy().into_owned();
        join(&cwd, path)
    };
    let trailing = combined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    let segments: Vec<&str> = combined.split('/').collect();
    let last = segments.len() - 1;
    let mut ended_on_dot = false;
    for (i, segment) in segments.iter().enumerate() {
        match *segment {
            "" => {}
            "." => ended_on_dot = i == last,
            ".." => {
                parts.pop();
                ended_on_dot = i == last;
            }
            s => parts.push(s),
        }
    }
    let mut full = format!("/{}", parts.join("/"));
    if trailing && !ended_on_dot && full != "/" {
        full.push('/');
    }
    Some(full)
}

/// `Directory.GetParent(full)?.FullName`: the path without its last segment (a trailing
/// separator counts as an empty last segment), None at the root.
fn parent_of(full: &str) -> Option<String> {
    if full == "/" {
        return None;
    }
    let cut = full.rfind('/')?;
    let parent = full[..cut].trim_end_matches('/');
    Some(if parent.is_empty() {
        "/".to_string()
    } else {
        parent.to_string()
    })
}

/// `Path.Join`: one separator between the two.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `Path.GetExtension`: from the last dot of the file name, "" when there is none or it ends
/// the name. A name that is all extension (".flac") is all extension.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => &name[i..],
        _ => "",
    }
}

/// Prove write access by writing, rather than inferring it from existence.
/// The whole point of the picker is to stop a user choosing a folder Octo
/// cannot download into, and Directory.Exists says nothing about that.
pub fn is_writable(dir: &str) -> bool {
    let probe = Path::new(dir).join(format!(".octo-write-probe-{}", uuid::Uuid::new_v4().simple()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            // DeleteOnClose.
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("octo-browse-")
            .tempdir()
            .expect("a temp dir")
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn the_reported_path_is_canonical_so_it_matches_what_was_listed() {
        let root = new_temp_dir();
        let child = root.path().join("child");
        std::fs::create_dir(&child).expect("creates");

        // Round-trips through the child and back up: the answer must describe
        // the directory actually listed, not the expression used to reach it.
        let result = DirectoryBrowser::new().browse(Some(&s(&child.join(".."))));

        assert!(result.exists);
        assert_eq!(result.path, s(root.path()));
        assert!(!result.path.contains(".."));
        assert!(result.entries.iter().any(|e| e.name == "child"));
    }

    #[test]
    fn listing_returns_subdirectories_and_never_file_names() {
        let root = new_temp_dir();
        std::fs::create_dir(root.path().join("albums")).expect("creates");
        std::fs::write(root.path().join("secret-track.flac"), "x").expect("writes");

        let result = DirectoryBrowser::new().browse(Some(&s(root.path())));

        assert!(result.entries.iter().any(|e| e.name == "albums"));
        assert!(!result.entries.iter().any(|e| e.name.ends_with(".flac")));
    }

    #[test]
    fn a_flat_library_reports_its_track_count_rather_than_looking_empty() {
        // The real library is ~2,350 loose tracks under four album folders. Listing
        // directories alone made it render as an almost-empty folder, so there was
        // no way to tell the right folder from a stray one.
        let root = new_temp_dir();
        std::fs::create_dir(root.path().join("Currents")).expect("creates");
        for n in ["a.flac", "b.mp3", "c.m4a", "d.opus", "cover.jpg", "notes.txt"] {
            std::fs::write(root.path().join(n), "x").expect("writes");
        }

        let result = DirectoryBrowser::new().browse(Some(&s(root.path())));

        assert_eq!(result.audio_files, 4);
        assert_eq!(result.entries.len(), 1); // still directories only
        assert!(!result.entries.iter().any(|e| e.name.contains('.')));
    }

    #[test]
    fn a_missing_path_is_reported_rather_than_throwing() {
        let missing = std::env::temp_dir().join(format!("octo-not-here-{}", uuid::Uuid::new_v4().simple()));
        let result = DirectoryBrowser::new().browse(Some(&s(&missing)));
        assert!(!result.exists);
        assert!(result.entries.is_empty());
        assert!(!result.writable);
    }

    #[test]
    fn writability_is_proved_by_writing_not_by_existence() {
        let root = new_temp_dir();
        let result = DirectoryBrowser::new().browse(Some(&s(root.path())));
        assert!(result.exists);
        assert!(result.writable);
        // The negative case cannot be forced portably (CI often runs as root, for whom mode
        // 0500 is no obstacle), so assert the probe leaves nothing behind instead. A probe
        // that littered the music library would be worse than no probe.
        assert_eq!(std::fs::read_dir(root.path()).expect("lists").count(), 0);
    }

    #[test]
    fn an_empty_path_starts_at_the_platform_root() {
        for path in [None, Some(""), Some("  ")] {
            let result = DirectoryBrowser::new().browse(path);
            assert_eq!(result.path, "/");
            assert!(result.exists);
            assert!(result.parent.is_none());
        }
    }

    #[test]
    fn parent_is_offered_for_navigation_and_is_null_at_the_top() {
        let root = new_temp_dir();
        let child = root.path().join("nested");
        std::fs::create_dir(&child).expect("creates");
        assert_eq!(
            DirectoryBrowser::new().browse(Some(&s(&child))).parent,
            Some(s(root.path()))
        );
        assert!(DirectoryBrowser::new().browse(Some("/")).parent.is_none());
    }

    // ---- Rust-only: .NET's path rules, as .NET 9 answered them --------------------------

    #[test]
    fn full_paths_and_parents_follow_dotnet() {
        let cases = [
            ("/tmp/x/", "/tmp/x/", Some("/tmp/x")),
            ("/tmp//x", "/tmp/x", Some("/tmp")),
            ("/tmp/./x/../y", "/tmp/y", Some("/tmp")),
            ("/", "/", None),
            ("//", "/", None),
            ("/tmp/x/..", "/tmp", Some("/")),
            ("/tmp/x/.", "/tmp/x", Some("/tmp")),
            ("/tmp/x//", "/tmp/x/", Some("/tmp/x")),
            ("/..", "/", None),
        ];
        for (input, full, parent) in cases {
            let got = get_full_path(input).expect("a path");
            assert_eq!(got, full, "{input}");
            assert_eq!(parent_of(&got).as_deref(), parent, "{input}");
        }
        assert!(get_full_path("/tmp/a\0b").is_none());
        let relative = get_full_path("rel/a").expect("a path");
        assert!(
            relative.starts_with('/') && relative.ends_with("/rel/a"),
            "{relative}"
        );
    }

    #[test]
    fn extensions_and_joins_follow_dotnet() {
        assert_eq!(get_extension("/a/b.FLAC"), ".FLAC");
        assert_eq!(get_extension("/a/.flac"), ".flac");
        assert_eq!(get_extension("/a/b."), "");
        assert_eq!(get_extension("/a.b/c"), "");
        assert_eq!(join("/", "bin"), "/bin");
        assert_eq!(join("/tmp/x/", "sub"), "/tmp/x/sub");
        assert_eq!(join("/tmp/x", "sub"), "/tmp/x/sub");
    }

    #[test]
    fn a_nul_path_is_rejected_as_it_was_given() {
        let result = DirectoryBrowser::new().browse(Some(" /tmp/a\0b "));
        assert_eq!(result.path, " /tmp/a\0b ");
        assert!(!result.exists && result.parent.is_none());
    }

    #[test]
    fn entries_sort_ignoring_case_and_the_list_is_capped() {
        let root = new_temp_dir();
        for n in ["beta", "Alpha", "gamma"] {
            std::fs::create_dir(root.path().join(n)).expect("creates");
        }
        let result = DirectoryBrowser::new().browse(Some(&s(root.path())));
        let names: Vec<&str> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "gamma"]);
        assert_eq!(result.entries[0].path, format!("{}/Alpha", s(root.path())));
        assert!(!result.truncated);

        let big = new_temp_dir();
        for i in 0..=DirectoryBrowser::MAX_ENTRIES {
            std::fs::create_dir(big.path().join(format!("d{i}"))).expect("creates");
        }
        std::fs::write(big.path().join("x.mp3"), "x").expect("writes");
        let result = DirectoryBrowser::new().browse(Some(&s(big.path())));
        assert!(result.truncated);
        assert_eq!(result.entries.len(), DirectoryBrowser::MAX_ENTRIES);
        assert_eq!(result.audio_files, 1, "counting goes on past the cap");
    }

    #[test]
    fn the_api_answer_is_camel_case() {
        let result = DirectoryBrowser::new().browse(Some("/definitely/not/here"));
        let json = octo_core::json::to_string(&result);
        assert_eq!(
            json,
            r#"{"path":"/definitely/not/here","parent":"/definitely/not","separator":"/","writable":false,"exists":false,"entries":[],"truncated":false,"audioFiles":0}"#
        );
    }
}
