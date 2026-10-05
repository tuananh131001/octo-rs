//! What every JSON state store under `/app/config` shares (state-files.md §3 and §6). Not a C#
//! file: each C# store repeated these few lines.
//!
//! - [`save_atomic`]: `File.WriteAllText(path + ".tmp", json)` then
//!   `File.Move(tmp, path, overwrite: true)`, creating the directory first. The temp name stays
//!   `<file>.tmp`, so a C# and a Rust process never trip over each other's leftovers.
//! - [`read_text`]: `File.Exists` + `File.ReadAllText`, which skips a UTF-8 BOM.
//! - [`write_atomic`], [`read_all_text`], [`lines`]: the same for bytes, for a file that must
//!   exist, and `File.ReadLines`' line splitting (the journals).
//! - [`flush_every`]: the `Timer` a coalescing store flushed from, plus the flush its `Dispose`
//!   did on a graceful shutdown, as a worker for the supervisor.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use tokio_util::sync::CancellationToken;

/// Writes `contents` to `path + ".tmp"` and renames it over `path`, creating the directory.
pub fn save_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// `<file>.tmp`, beside the file.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// The file's text, or None when it does not exist. A leading BOM is dropped, as
/// `File.ReadAllText` did.
pub fn read_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(match text.strip_prefix('\u{FEFF}') {
                Some(rest) => rest.to_string(),
                None => text,
            }))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

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

/// `File.ReadAllText` for a file the caller knows exists: UTF-8, a byte order mark skipped, bytes that are not UTF-8 read as U+FFFD.
pub fn read_all_text(path: &Path) -> io::Result<String> {
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

/// For a C# property that is a non-nullable reference type (a `string` or a `List<T>`):
/// System.Text.Json read a JSON `null` into it without complaint, where serde would fail the
/// whole file. Read `null` as the default instead.
pub fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A coalescing store's flush timer: calls `flush` every `interval` (the first call one interval
/// in, as `new Timer(_ => Flush(), null, interval, interval)` did), and once more when `token`
/// is cancelled, standing in for the flush `Dispose()` did on a graceful shutdown. The flush
/// runs on the blocking pool: a store can be a few megabytes of JSON.
pub async fn flush_every(
    interval: Duration,
    token: CancellationToken,
    flush: Arc<dyn Fn() + Send + Sync>,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let flush = flush.clone();
                tokio::task::spawn_blocking(move || flush()).await?;
            }
            _ = token.cancelled() => {
                let flush = flush.clone();
                tokio::task::spawn_blocking(move || flush()).await?;
                return Ok(());
            }
        }
    }
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
    fn write_atomic_leaves_no_temp_file_and_read_all_text_skips_the_bom() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("deeper").join("state.json");
        write_atomic(&path, b"\xEF\xBB\xBF{}").expect("written");
        assert!(!temp_path(&path).exists());
        assert_eq!(read_all_text(&path).expect("read"), "{}");
    }
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn save_atomic_creates_the_directory_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("nested").join("state.json");
        save_atomic(&path, "[1]").expect("saves");
        save_atomic(&path, "[2]").expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "[2]");
        assert!(!tmp_path(&path).exists());
        assert_eq!(
            tmp_path(&path).file_name().and_then(|n| n.to_str()),
            Some("state.json.tmp")
        );
    }

    #[test]
    fn read_text_skips_a_bom_and_answers_none_for_a_missing_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("a.json");
        assert!(read_text(&path).expect("no error").is_none());
        std::fs::write(&path, "\u{FEFF}[]").expect("writes");
        assert_eq!(read_text(&path).expect("reads").as_deref(), Some("[]"));
    }

    #[tokio::test(start_paused = true)]
    async fn flush_every_ticks_and_flushes_once_more_on_shutdown() {
        let calls = Arc::new(AtomicU32::new(0));
        let token = CancellationToken::new();
        let c = calls.clone();
        let task = tokio::spawn(flush_every(
            Duration::from_secs(10),
            token.clone(),
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        ));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the first flush is one interval in"
        );
        tokio::time::sleep(Duration::from_secs(21)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        token.cancel();
        task.await.expect("joins").expect("ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
