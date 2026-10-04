//! What every state store under `/app/config` does with its file, as the C# stores each did
//! inline: write to `<file>.tmp` and rename it over the file (`File.WriteAllText` then
//! `File.Move(tmp, path, overwrite: true)`), read text as `File.ReadAllText` did, and split
//! lines as `File.ReadLines` did. Not a C# file (state-files.md §6).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

/// `<path>.tmp`, the temporary name every C# store used, kept so a C# and a Rust process never
/// trip over each other's leftovers.
pub fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    PathBuf::from(temp)
}

/// Creates the file's folder, writes `<path>.tmp` and renames it over `path`. No byte order
/// mark, as `File.WriteAllText` wrote none.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let temp = temp_path(path);
    std::fs::write(&temp, contents)?;
    std::fs::rename(&temp, path)
}

/// `File.ReadAllText`: UTF-8, a byte order mark skipped, bytes that are not UTF-8 read as U+FFFD.
pub fn read_text(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `File.ReadLines` / `ReadAllLines`: lines end at "\r\n", "\n" or "\r", and a final line
/// ending does not start another line.
pub fn lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find(['\r', '\n']) {
            Some(end) => {
                lines.push(&rest[..end]);
                let skip = if rest[end..].starts_with("\r\n") { 2 } else { 1 };
                rest = &rest[end + skip..];
            }
            None => {
                lines.push(rest);
                break;
            }
        }
    }
    lines
}

/// A JSON `null` where C# declared a non-nullable string or list: STJ stored the null, and the
/// port reads it as the empty value instead.
pub fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_as_dotnet_did() {
        assert_eq!(lines("a\r\nb\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(lines("a\n"), ["a"]);
        assert_eq!(lines("a\n\nb"), ["a", "", "b"]);
        assert!(lines("").is_empty());
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_and_read_text_skips_the_bom() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("deeper").join("state.json");
        write_atomic(&path, b"\xEF\xBB\xBF{}").expect("written");
        assert!(!temp_path(&path).exists());
        assert_eq!(read_text(&path).expect("read"), "{}");
    }
}
