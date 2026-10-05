//! Port of `Services/Lyrics/LyricsLibrarySteps.cs`, the other half of the C# partial class
//! `LyricsLibraryWorker`: the lyrics page's steps (see `LyricsLibraryMode`): scan, preview, save,
//! undo. A scan reads tags only, so it is quick and changes nothing; a preview looks the picked
//! songs up one at a time with the same pause as a walk; Save writes only what the preview found,
//! and only where Octo may; Undo puts back every file Save wrote over, newest first.
//!
//! The rules without the files and the lookups are `octo_core::lyrics::lyrics_library_steps`.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use chrono::Utc;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::lyrics::lyrics_library_job::{OCTO_DOWNLOADS, WHOLE_LIBRARY};
use octo_core::lyrics::lyrics_library_steps::{
    self as steps, SERVICES_STOPPED_ANSWERING, better_than, has_name, picked, scan_row, undo_reason,
    unsave_rows,
};
use octo_core::lyrics::{
    LyricsLibraryMode, LyricsLibraryRequest, LyricsLibraryRow, LyricsLibraryRun, LyricsLibraryStatus,
    LyricsText, SongLyrics, SongLyricsPlace,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::lyrics_choices::LyricsChoiceService;
use super::lyrics_library_job::{LyricsLibraryWorker, count, new_run_id};
use super::lyrics_sidecar_writer::LyricsJob;
use super::song_lyrics;

impl LyricsLibraryWorker {
    pub(super) async fn run_step(
        &self,
        request: &LyricsLibraryRequest,
        mode: LyricsLibraryMode,
        stopping: &CancellationToken,
    ) -> anyhow::Result<()> {
        self.cancel_requested.store(false, Ordering::SeqCst);
        if request.resume && self.store.read(LyricsLibraryRun::can_resume) {
            self.store.update(|run| {
                run.status = LyricsLibraryStatus::Running;
                run.reason = None;
            });
            let (cursor, total) = self.store.read(|run| (run.cursor, run.queue.len()));
            info!("Lyrics page {mode:?}, resuming at {cursor}/{total}");
        } else if !self.start_step(request, mode).await? {
            return Ok(());
        }

        match mode {
            LyricsLibraryMode::Scan => self.scan(stopping),
            LyricsLibraryMode::Preview => self.preview(stopping).await,
            LyricsLibraryMode::Save => self.save(stopping).await,
            LyricsLibraryMode::Undo => self.undo(),
            LyricsLibraryMode::Walk => Ok(()),
        }
    }

    /// A new run for the step, keeping the list the earlier steps made. False when there is
    /// nothing to do, which is reported on the run.
    async fn start_step(
        &self,
        request: &LyricsLibraryRequest,
        mode: LyricsLibraryMode,
    ) -> anyhow::Result<bool> {
        let current = self.store.current();
        let mut run = LyricsLibraryRun {
            run_id: new_run_id(),
            status: LyricsLibraryStatus::Running,
            mode,
            scope: current.scope,
            started_utc: Some(Utc::now()),
            rows: current.rows,
            word_already: current.word_already,
            review: current.review,
            ..LyricsLibraryRun::default()
        };
        match mode {
            LyricsLibraryMode::Scan => {
                let whole = request
                    .scope
                    .as_deref()
                    .is_some_and(|scope| eq_ignore_case(scope, WHOLE_LIBRARY));
                run.scope = if whole { WHOLE_LIBRARY } else { OCTO_DOWNLOADS }.to_string();
                run.rows = Vec::new();
                run.word_already = 0;
                self.store.replace(run);
                // The C# listed the songs into the run it had just put in the store, then put it
                // there again: whatever happened to the run meanwhile (a review entry dismissed)
                // stays.
                let queue = self.enumerate(whole).await;
                let total = count(queue.len());
                self.store.update(|run| {
                    run.queue = queue;
                    run.total = total;
                });
                self.store.flush();
                let scope = self.store.read(|run| run.scope.clone());
                info!("Lyrics page {mode:?}: {total} item(s), {scope}");
                return Ok(true);
            }
            LyricsLibraryMode::Preview | LyricsLibraryMode::Save => {
                let chosen = picked(&run.rows, request.picked.as_deref(), mode);
                run.queue = chosen.clone();
                run.picked = Some(chosen);
            }
            LyricsLibraryMode::Undo | LyricsLibraryMode::Walk => run.queue = Vec::new(),
        }
        run.total = if mode == LyricsLibraryMode::Undo {
            count(self.journal.read_all()?.len())
        } else {
            count(run.queue.len())
        };
        let (total, scope) = (run.total, run.scope.clone());
        self.store.replace(run);
        info!("Lyrics page {mode:?}: {total} item(s), {scope}");
        if total > 0 {
            return Ok(true);
        }
        self.finish(Some(
            if mode == LyricsLibraryMode::Undo {
                "There was nothing to put back."
            } else {
                "Nothing was picked."
            }
            .to_string(),
        ));
        Ok(false)
    }

    fn finish(&self, reason: Option<String>) {
        self.store.update(|run| steps::finish(run, reason, Utc::now()));
    }

    /// Stops the loop when asked to, saying so on the run. True when it should stop.
    fn stopping(&self, stopping: &CancellationToken) -> bool {
        if stopping.is_cancelled() {
            self.store
                .update(|run| run.status = LyricsLibraryStatus::Interrupted);
            return true;
        }
        if !self.cancel_requested.load(Ordering::SeqCst) {
            return false;
        }
        self.store.update(|run| steps::cancel(run, Utc::now()));
        true
    }

    /// The rows by id, as the C# `ToDictionary` made them: two rows with one id are an error.
    fn rows_by_id(&self) -> anyhow::Result<HashMap<String, LyricsLibraryRow>> {
        self.store.read(|run| {
            let mut rows = HashMap::with_capacity(run.rows.len());
            for row in &run.rows {
                if rows.insert(row.id.clone(), row.clone()).is_some() {
                    return Err(anyhow!(
                        "An item with the same key has already been added. Key: {}",
                        row.id
                    ));
                }
            }
            Ok(rows)
        })
    }

    // ---- Scan ---------------------------------------------------------------------------------

    fn scan(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        let (queue, start) = self
            .store
            .read(|run| (run.queue.clone(), run.cursor.max(0) as usize));
        for (index, path) in queue.iter().enumerate().skip(start) {
            if self.stopping(stopping) {
                return Ok(());
            }
            let scanned = self.scan_song(path);
            self.store.update(|run| {
                run.cursor = count(index) + 1;
                run.processed += 1;
                run.last_path = Some(path.clone());
                match scanned {
                    Err(error) => {
                        run.failed += 1;
                        run.errors.push(format!("{path}: {error}"));
                    }
                    Ok((Some(row), _)) => run.rows.push(row),
                    Ok((None, true)) => run.word_already += 1,
                    Ok((None, false)) => run.skipped += 1,
                }
            });
        }
        self.finish(None);
        Ok(())
    }

    /// One song as a scan sees it: a row when its lyrics are missing, plain or timed by line;
    /// word true when it already has word timing. Neither for a song without an artist and a
    /// title to look it up by, or with a lyrics file Octo does not read. An I/O error is the
    /// caller's, which counts the song as failed.
    pub fn scan_song(&self, path: &str) -> io::Result<(Option<LyricsLibraryRow>, bool)> {
        let Some(tags) = self.writer.tags().read_song(Path::new(path))? else {
            return Ok((None, false));
        };
        let artist = tags.first_performer.or(tags.first_album_artist);
        Ok(scan_row(
            path,
            artist.as_deref(),
            tags.title.as_deref(),
            tags.album.as_deref(),
            || song_lyrics::of_with_tag_lyrics(Path::new(path), tags.lyrics.as_deref()),
        ))
    }

    // ---- Preview ------------------------------------------------------------------------------

    async fn preview(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        let (queue, start) = self
            .store
            .read(|run| (run.queue.clone(), run.cursor.max(0) as usize));
        let rows = self.rows_by_id()?;
        let mut busy_in_a_row = 0;
        for (index, id) in queue.iter().enumerate().skip(start) {
            if self.stopping(stopping) {
                return Ok(());
            }
            let Some(row) = rows.get(id) else {
                continue;
            };

            let job = self.read_tags(&row.path).unwrap_or_else(|| {
                LyricsJob::new(
                    &row.path,
                    &row.artist,
                    LyricsText::query_title(&row.title, &row.artist),
                    row.album.clone(),
                    None,
                )
            });
            let exists = Path::new(&row.path).is_file();
            let has = if exists {
                self.writer.song_lyrics(Path::new(&row.path))
            } else {
                SongLyrics::NOTHING
            };
            // The C# caught a lookup's exception as the song's error, and a cancellation as an
            // interruption. Neither reaches here: the lyrics service answers a source that throws
            // as "not now", and a cancelled lookup as "not now" too (counted as busy before the
            // pause ends the run), so only a song that is gone fails.
            let lookup = if exists {
                Some(self.writer.look_up(&job, has.timing, stopping).await)
            } else {
                None
            };

            let busy = lookup.as_ref().is_some_and(|lookup| lookup.transient);
            busy_in_a_row = if busy { busy_in_a_row + 1 } else { 0 };
            if busy_in_a_row >= Self::BUSY_IN_A_ROW_LIMIT {
                let from = count(index) - Self::BUSY_IN_A_ROW_LIMIT + 1;
                self.store.update(|run| {
                    run.status = LyricsLibraryStatus::Interrupted;
                    run.cursor = from;
                    run.reason = Some(SERVICES_STOPPED_ANSWERING.to_string());
                });
                return Ok(());
            }

            let found = lookup
                .as_ref()
                .and_then(|lookup| better_than(lookup.result.as_ref(), has.timing));
            self.store.update(|run| {
                run.cursor = count(index) + 1;
                run.processed += 1;
                run.last_path = Some(row.path.clone());
                let Some(row) = run.rows.iter_mut().find(|live| live.id == *id) else {
                    return;
                };
                row.has = has_name(has.timing).to_string();
                match (&lookup, &found) {
                    (None, _) => {
                        row.result = "failed".to_string();
                        run.failed += 1;
                        run.errors.push(format!("{}: the file is gone", row.path));
                    }
                    _ if busy => {
                        row.result = "busy".to_string();
                        run.busy += 1;
                    }
                    (_, Some(found)) => {
                        row.result = "found".to_string();
                        row.source = Some(found.source.clone());
                        row.kind = Some(LyricsChoiceService::kind_of(found).to_string());
                        row.candidate_id = found.candidate_id.clone();
                        row.doubt = found.doubt.clone();
                        row.preview = LyricsText::preview(found, 2);
                        row.found_synced = found.synced.clone();
                        row.found_plain = found.plain.clone();
                        run.upgraded += 1;
                    }
                    (Some(lookup), None) => {
                        row.result = "none".to_string();
                        row.kind = lookup
                            .result
                            .as_ref()
                            .is_some_and(|result| result.instrumental)
                            .then(|| "instrumental".to_string());
                        row.found_synced = None;
                        row.found_plain = None;
                        run.not_found += 1;
                    }
                }
            });
            self.pause(stopping).await?;
        }
        self.finish(None);
        Ok(())
    }

    // ---- Save ---------------------------------------------------------------------------------

    async fn save(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        let (queue, start, run_id) = self
            .store
            .read(|run| (run.queue.clone(), run.cursor.max(0) as usize, run.run_id.clone()));
        let rows = self.rows_by_id()?;
        let record = |path: &Path, kind: &str, before: Option<&str>| -> anyhow::Result<()> {
            Ok(self
                .journal
                .record(&path.to_string_lossy(), kind, before, &run_id)?)
        };
        for (index, id) in queue.iter().enumerate().skip(start) {
            if self.stopping(stopping) {
                return Ok(());
            }
            let Some(row) = rows.get(id) else {
                continue;
            };
            let Some(found) = row.found() else {
                continue;
            };

            let path = Path::new(&row.path);
            let (result, error) = if !path.is_file() {
                ("failed", Some("the file is gone".to_string()))
            } else {
                let has = self.writer.song_lyrics(path);
                if has.place != SongLyricsPlace::None && has.timing >= found.timing() {
                    ("kept", None)
                } else {
                    match self.writer.save_chosen(path, &found, &record, stopping).await {
                        Ok(true) => ("saved", None),
                        Ok(false) => ("blocked", None),
                        Err(error) => ("failed", Some(error.to_string())),
                    }
                }
            };
            self.store.update(|run| {
                run.cursor = count(index) + 1;
                run.processed += 1;
                run.last_path = Some(row.path.clone());
                if let Some(live) = run.rows.iter_mut().find(|live| live.id == *id) {
                    live.result = result.to_string();
                }
                match result {
                    "saved" => run.written += 1,
                    "kept" => run.already_had += 1,
                    "blocked" => run.skipped += 1,
                    _ => {
                        run.failed += 1;
                        run.errors
                            .push(format!("{}: {}", row.path, error.as_deref().unwrap_or("")));
                    }
                }
            });
        }
        self.finish(None);
        Ok(())
    }

    // ---- Undo ---------------------------------------------------------------------------------

    fn undo(&self) -> anyhow::Result<()> {
        let entries = self.journal.read_all()?;
        let mut restored = 0;
        let mut left = 0;
        for entry in entries.iter().rev() {
            if self.writer.restore(entry) {
                restored += 1;
            } else {
                left += 1;
            }
        }
        self.journal.clear()?;
        self.store.update(|run| {
            run.processed = count(entries.len());
            run.written = restored;
            run.skipped = left;
            unsave_rows(run);
        });
        info!("Lyrics page undo: {restored} put back, {left} changed since and left alone");
        self.finish(undo_reason(left as usize));
        Ok(())
    }
}

#[cfg(test)]
#[path = "lyrics_library_steps_tests.rs"]
mod tests;
