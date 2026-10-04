//! Port of `Services/LastFm/LastFmRadioTrackCache.cs`: `<cache>/radio/<key>.mp3` and the
//! measured profile beside each (`<key>.mp3.json`, state-files.md §4.25).
//!
//! Bounded temporary storage for fully transcoded Radio starter tracks. This is deliberately the
//! normal Octo cache, not the music library: preparing a station must never turn listening into
//! a permanent acquisition.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use octo_core::common::{PathHelper, SingleFlight, dotnet};
use octo_core::last_fm::RadioAudioProfile;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::OperationCanceled;

const MAXIMUM_BYTES: u64 = 512 * 1024 * 1024;
const RETENTION: TimeDelta = TimeDelta::hours(24);
const PRUNE_INTERVAL: TimeDelta = TimeDelta::hours(1);
const PRODUCE_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// What fills a cache entry: it is handed the temporary file and the work's own token (the
/// 15-minute deadline, never a caller's), writes the MP3, and hands the file back so the cache
/// can finish writing it.
pub type Producer = Box<
    dyn FnOnce(tokio::fs::File, CancellationToken) -> BoxFuture<'static, anyhow::Result<tokio::fs::File>>
        + Send,
>;

pub struct LastFmRadioTrackCache {
    root: PathBuf,
    single_flight: SingleFlight<String, PathBuf>,
    transcode_slots: Arc<Semaphore>,
    /// `_nextPruneUtc`, under `_pruneLock`.
    next_prune_utc: Mutex<Option<DateTime<Utc>>>,
}

impl Default for LastFmRadioTrackCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LastFmRadioTrackCache {
    /// `<temp>/octo-cache/radio`.
    pub fn new() -> Self {
        Self::with_root(Path::new(&PathHelper::get_cache_path()).join("radio"))
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        LastFmRadioTrackCache {
            root: root.into(),
            single_flight: SingleFlight::new(),
            transcode_slots: Arc::new(Semaphore::new(2)),
            next_prune_utc: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The lower-case hex SHA-256 of `"{USERNAME}\n{stationId}\n{trackIdentity}\n{bitrate}"`.
    pub fn key(&self, username: &str, station_id: &str, track_identity: &str, bitrate_kbps: i32) -> String {
        let material = [
            dotnet::to_upper_invariant(username.trim()),
            station_id.to_string(),
            track_identity.to_string(),
            bitrate_kbps.to_string(),
        ]
        .join("\n");
        hex::encode(Sha256::digest(material.as_bytes()))
    }

    /// The cached MP3 for `key`, produced once however many callers ask at the same time. A
    /// caller that cancels stops waiting; the production carries on for the next caller.
    pub async fn get_or_create(
        self: &Arc<Self>,
        key: &str,
        producer: Producer,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<PathBuf> {
        tokio::fs::create_dir_all(&self.root).await?;
        let path = self.root.join(format!("{key}.mp3"));
        if is_ready(&path) {
            return Ok(touch(path));
        }

        let cache = Arc::clone(self);
        let key_owned = key.to_string();
        let work = self.single_flight.run(
            key.to_string(),
            move |token| async move {
                if is_ready(&path) {
                    return Ok(touch(path));
                }
                let permit = tokio::select! {
                    biased;
                    () = token.cancelled() => return Err(OperationCanceled.into()),
                    permit = Arc::clone(&cache.transcode_slots).acquire_owned() => {
                        permit.expect("the slots are never closed")
                    }
                };
                let temporary_path = cache
                    .root
                    .join(format!(".{key_owned}.{}.tmp", uuid::Uuid::new_v4().simple()));
                let result = async {
                    let output = tokio::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary_path)
                        .await?;
                    let mut output = producer(output, token).await?;
                    output.flush().await?;
                    drop(output);
                    if !is_ready(&temporary_path) {
                        anyhow::bail!("Radio starter transcode produced no audio");
                    }
                    std::fs::rename(&temporary_path, &path)?;
                    cache.prune_if_due(&path);
                    Ok(path)
                }
                .await;
                drop(permit);
                // Cleanup is best effort.
                if temporary_path.exists() {
                    let _ = std::fs::remove_file(&temporary_path);
                }
                result
            },
            PRODUCE_DEADLINE,
        );
        tokio::select! {
            biased;
            () = cancellation_token.cancelled() => Err(OperationCanceled.into()),
            result = work => result.map_err(|error| anyhow::anyhow!("{error:#}")),
        }
    }

    pub fn get_ready_path(&self, key: &str) -> Option<PathBuf> {
        let path = self.root.join(format!("{key}.mp3"));
        is_ready(&path).then(|| touch(path))
    }

    pub async fn open_read(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        touch(path.to_path_buf());
        tokio::fs::File::open(path).await
    }

    pub fn is_ready_path(&self, path: &Path) -> bool {
        is_ready(path)
    }

    /// The measured profile of a cached track sits beside it as JSON. Absent (an older cache, or
    /// a measurement that failed) means "unknown", never an error.
    pub fn get_profile(&self, path: &Path) -> Option<RadioAudioProfile> {
        let sidecar = profile_path(path);
        if !sidecar.is_file() {
            return None;
        }
        let text = crate::services::state_file::read_all_text(&sidecar).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save_profile(&self, path: &Path, profile: &RadioAudioProfile) {
        let sidecar = profile_path(path);
        let mut temporary = sidecar.as_os_str().to_owned();
        temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let temporary = PathBuf::from(temporary);
        // The profile is an optimisation; the audio is what matters.
        let written = std::fs::write(&temporary, octo_core::json::to_string(profile))
            .and_then(|()| std::fs::rename(&temporary, &sidecar));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }

    fn prune_if_due(&self, protected_path: &Path) {
        let mut next_prune = self.next_prune_utc.lock();
        let now = Utc::now();
        if next_prune.is_some_and(|next| now < next) {
            return;
        }
        *next_prune = Some(now + PRUNE_INTERVAL);
        let cutoff = SystemTime::now() - Duration::from_secs(RETENTION.num_seconds() as u64);
        for (file, accessed, _) in mp3_files(&self.root) {
            if file != protected_path && accessed < cutoff {
                try_delete(&file);
            }
        }
        let remaining = mp3_files(&self.root);
        let mut total: u64 = remaining.iter().map(|(_, _, length)| length).sum();
        for (file, _, length) in remaining {
            if total <= MAXIMUM_BYTES {
                break;
            }
            if file == protected_path {
                continue;
            }
            if try_delete(&file) {
                total -= length;
            }
        }
    }
}

/// The `*.mp3` files in the folder with their access times and lengths, least recently used
/// first.
fn mp3_files(root: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime, u64)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".mp3"))
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    entry.path(),
                    metadata.accessed().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len(),
                )
            })
        })
        .collect();
    files.sort_by_key(|(_, accessed, _)| *accessed);
    files
}

fn profile_path(path: &Path) -> PathBuf {
    let mut sidecar = path.as_os_str().to_owned();
    sidecar.push(".json");
    PathBuf::from(sidecar)
}

fn is_ready(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn touch(path: PathBuf) -> PathBuf {
    // Access-time support varies by filesystem.
    if let Ok(file) = std::fs::File::open(&path) {
        let _ = file.set_times(std::fs::FileTimes::new().set_accessed(SystemTime::now()));
    }
    path
}

fn try_delete(file: &Path) -> bool {
    if std::fs::remove_file(file).is_err() {
        return false;
    }
    let sidecar = profile_path(file);
    if sidecar.exists() && std::fs::remove_file(&sidecar).is_err() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn writing(bytes: &'static [u8]) -> Producer {
        Box::new(move |mut file, _| {
            async move {
                file.write_all(bytes).await?;
                Ok(file)
            }
            .boxed()
        })
    }

    #[test]
    fn keys_are_the_hash_of_the_listener_station_track_and_bitrate() {
        let cache = LastFmRadioTrackCache::with_root("/nowhere");
        let key = cache.key(" alice ", "", "abc", 192);
        assert_eq!(key, hex::encode(Sha256::digest(b"ALICE\n\nabc\n192")));
        assert_eq!(key.len(), 64);
    }

    /// Rust-only: one production for two callers, an empty answer is refused and leaves nothing
    /// behind, and a profile is read back.
    #[tokio::test]
    async fn a_track_is_produced_once_and_an_empty_one_is_refused() {
        let dir = TempDir::new().unwrap();
        let cache = Arc::new(LastFmRadioTrackCache::with_root(dir.path().join("radio")));
        let none = CancellationToken::new();
        let (first, second) = tokio::join!(
            cache.get_or_create("k1", writing(b"ID3 first"), &none),
            cache.get_or_create("k1", writing(b"second"), &none),
        );
        let path = first.unwrap();
        assert_eq!(second.unwrap(), path);
        // Whichever caller got there first produced it; the other joined.
        let produced = std::fs::read(&path).unwrap();
        assert!(produced == b"ID3 first" || produced == b"second", "{produced:?}");
        assert_eq!(cache.get_ready_path("k1"), Some(path.clone()));
        assert!(cache.is_ready_path(&path));

        let error = cache.get_or_create("k2", writing(b""), &none).await.unwrap_err();
        assert!(error.to_string().contains("produced no audio"), "{error}");
        assert!(cache.get_ready_path("k2").is_none());
        let leftovers = std::fs::read_dir(dir.path().join("radio")).unwrap().count();
        assert_eq!(leftovers, 1);

        assert!(cache.get_profile(&path).is_none());
        let profile = RadioAudioProfile::new(-13.0, 6.0, -1.0, -3.0, 2000.0, 0.1, 6000.0)
            .with_kinship(Some("Rock"), Some(vec!["rock".into()]));
        cache.save_profile(&path, &profile);
        assert_eq!(cache.get_profile(&path), Some(profile));
        std::fs::write(profile_path(&path), "not json").unwrap();
        assert!(cache.get_profile(&path).is_none());

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = cache
            .get_or_create("k3", writing(b"x"), &cancelled)
            .await
            .unwrap_err();
        assert!(error.is::<OperationCanceled>());
    }
}
