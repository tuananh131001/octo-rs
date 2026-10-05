# Port upstream 2026.10.04 to the Rust workspace

Source: <https://github.com/winters27/octo/compare/2026.10.03.2...2026.10.04>. It has two commits:
`3f14a881` fix(lyrics) and `89a4be19` release: 2026.10.04.

The upstream change touches these C# files: `LyricsAdminController.cs`,
`CoverArt/CoverUpgrade.cs`, the new `Library/NavidromeSongList.cs`, `Lyrics/LyricsLibrarySteps.cs`,
`wwwroot/admin/admin.js`, `octo.csproj` (version), and the new test `LyricsPageStatusTests.cs`.

Decisions made while planning:
- **Parity:** follow upstream. Rename the member to `running`, update the known-diffs row and
  allowlist the `09-admin/lyrics-library-busy-collision` case against the frozen 2026.10.03.2
  recording. Don't re-record the C# baseline.
- **Version:** bump `VERSION` to `2026.10.04` in its own release commit. Docs that name
  2026.10.03.2 as the C# *baseline* stay as they are.

## Goal

Make the Rust port behave like upstream release 2026.10.04:

1. The Lyrics page status (`GET /api/admin/lyrics/library`) is written again. The running flag
   is called `running` and `busy` stays the count of songs no service answered for.
2. A lyrics scan takes artist, title, album and tag lyrics from Navidrome's song list, and
   finds lyrics files with one listing of the music root. It opens only songs that already have
   lyrics, or songs Navidrome doesn't name.
3. The Navidrome song listing is shared between the lyrics scan and the cover upgrade.
4. The admin UI (`crates/octo/wwwroot/admin/admin.js`) takes the upstream page changes. It
   shows "Getting started" right away, keeps watching when a status request fails, asks you to
   sign in on a 401, moves messages under the summary line, shows a scan's progress and time
   left, and estimates how long "Find better lyrics" will take.
5. The release is 2026.10.04.

This is a port of upstream behaviour (CONVENTIONS.md: "port behaviour, not design"). C# doc
comments and inline comments carry across as `///` and `//`.

## Tasks

### 1. Shared Navidrome song list
- [x] Add `crates/octo/src/services/library/navidrome_song_list.rs`, from `NavidromeSongList.cs`:
  - `NavidromeSongEntry { full_path, album_id, artist, title, album, lyrics }` with
    `has_tag_lyrics()`. It is true when lyrics is non-blank and its trimmed value is not `[]`
    or `null`.
  - `pub async fn list(identity, http, base_url, root) -> HashMap<String, NavidromeSongEntry>`:
    `GET /api/song?_start&_end&_sort=id&_order=ASC`, 1000 a page, capped at 200,000, with the
    header `X-Nd-Authorization: Bearer <admin jwt>`.
  - Each song is keyed under `libraryPath + relative` and `root + relative`, and under the
    full path when Navidrome reports a rooted path (older Navidrome). The first key added wins
    (`TryAdd`).
  - It stops on a non-success answer, a non-array body or a short page. Any error is logged
    at info ("Could not list Navidrome's songs, so each file is read instead: …") and gives
    whatever was collected so far.
  - Unlike the old cover code, a song with no `albumId` is still listed.
- [x] Register it in `services/library/mod.rs`.
- [x] Reuse the path-joining and full-path helpers that `cover_upgrade::navidrome_albums`
  already uses, so keys come out byte-identical to today's.

### 2. Cover upgrade uses the shared list
- [x] Change `CoverUpgrade::navidrome_albums` (`crates/octo/src/services/cover_art/cover_upgrade.rs:1543`)
  to call `navidrome_song_list::list`. Keep only entries with a non-empty `album_id`, mapped
  path → album id.
- [x] The log message changes to the shared one, as upstream did.
- [x] Existing `cover_upgrade_tests.rs` cases that stub `/api/song` still pass unchanged.

### 3. Lyrics scan reads Navidrome first
- [x] Give `LyricsLibraryWorker` (`crates/octo/src/services/lyrics/lyrics_library_job.rs:171`)
  what the list needs: optional `NavidromeIdentityService`, `reqwest::Client` and the Subsonic
  URL (read from `settings`). Wire them in `crates/octo/src/app.rs:999` and in the test
  constructors (`lyrics_library_job_tests.rs`, `lyrics_library_steps_tests.rs`). Tests pass
  `None` so they keep the file-by-file path.
- [x] `lyrics_library_steps.rs` `scan`: make it async, matching the C# change from
  `Task ScanAsync` to `async`. At the start, fetch the Navidrome list and compute
  `lyrics_file_stems(root)`. Log at info: "Lyrics scan: Navidrome named {known} of {total}
  song(s); {files} lyrics file(s) found", with `-1` when the stems are unknown. Update the
  caller in the step dispatcher.
- [x] `pub(crate) fn lyrics_file_stems(root) -> Option<HashSet<String>>`: walk the root
  recursively, keep files whose extension is `.lrc .txt .ttml .elrc .srt .yaml .yml`
  (case-insensitive), and map each to its full-path stem (directory plus file name without the
  extension). Return `None` when the root is missing or the walk fails.
- [x] `scan_song_known(path, known: Option<&NavidromeSongEntry>, lyrics_files: Option<&HashSet<String>>)`:
  - Fall back to the existing `scan_song(path)` in any of these cases: `known` is `None`,
    artist or title is blank, `has_tag_lyrics()` is true, `lyrics_files` is `None`, or the
    stems contain this song's stem.
  - Otherwise return a row without opening the file: `id = LyricsLibraryRow::id_of(path)`,
    trimmed artist and title, album trimmed (or `None` if blank), `has = "none"`, and
    `word = false`.
  - Look entries up by the song's full path. This mirrors `Path.GetFullPath`: normalise the
    path without touching the disk, using the helper the cover upgrade already uses.

### 4. Lyrics status endpoint: `busy` → `running`
- [x] `crates/octo/src/controllers/admin/lyrics_admin.rs:174`: rename the second `busy` to
  `running`. Replace the doc comment above `get_library_run` with the upstream comment ("Not
  'busy': that is the run's count of songs no service answered for…").
- [x] Rewrite `the_lyrics_run_answers_the_busy_collision` (`controllers/admin/tests_6b2.rs:727`)
  as a status test, porting `LyricsPageStatusTests.LyricsStatus_WithRowsAndABusyCount_IsWritten`:
  - The test seeds a completed Preview run with `busy = 2` and a row whose `found_synced`
    holds "secret text".
  - Signed in, the endpoint answers 200 with `busy == 2`, `running == false` and
    `rows[0].result == "found"`, and the body does not contain "secret text".
  - Signed out, it still answers 401.
- [x] Port `CoverStatus_IsWritten`: signed in, `GET /api/admin/covers/upgrade` answers 200.

### 5. Port the scan unit tests
In `lyrics_library_steps_tests.rs`:
- [x] `scan_a_song_navidrome_names_with_no_lyrics_is_listed_without_opening_it`. The file does
  not exist, so getting a row back proves the scan never opened it. Expect `has = "none"` and
  the given artist, title and album.
- [x] `scan_a_song_with_lyrics_beside_it_is_opened_to_learn_their_timing`. Write a tagged MP3
  fixture with a word-timed `.lrc` beside it. Expect the stems to contain the song's stem, no
  row, and `word = true`.
- [x] `scan_tag_lyrics_navidrome_read_are_opened`: check the three `has_tag_lyrics` cases
  (`[{"synced":true}]`, `[]` and `null`).
- [x] A worker-level scan test with a stubbed `/api/song`. Most songs become rows without
  being opened, and the summary counts match the file-by-file scan.

### 6. Admin UI
- [x] Apply the upstream `octo/wwwroot/admin/admin.js` hunks to `crates/octo/wwwroot/admin/admin.js`.
  The file is identical to `csharp-final`, so the patch should apply cleanly:
  - `coverNote` and `lyricsNote` write into `.cover-summary`.
  - Counts use `toLocaleString()`.
  - The "Find better lyrics" hint gives a time estimate at 2.5 s a song.
  - Progress shows for Scan and Preview, and the time left is computed for every mode.
  - New `lyricsExpectRunning`, `lyricsLoadError`, `lyricsWatch` and `renderLyricsTrouble`.
  - `loadLyricsLibrary` survives fetch errors and reads `run.running`.
  - `startLyrics` renders the starting state right away.
- [x] Rebuild anything derived from wwwroot (static compressed variants or ETags, if the
  build or tests precompute them).

### 7. Parity and docs
- [x] `docs/rust-migration/known-diffs.md:138`: rewrite the row. Upstream 2026.10.04 renamed
  the member to `running`, and Rust follows, so the endpoint answers 200 where the 2026.10.03.2
  baseline answered 400.
- [x] Add a known-diffs row for the scan: when Navidrome names a song with no lyrics, its row
  comes from Navidrome's tags rather than the file's. Upstream does this too, but the C#
  baseline recording didn't.
- [x] `parity/allowlist.json`: add an entry for `09-admin/lyrics-library-busy-collision`
  (status and body), with a `knownDiff` and a `reason` that point at upstream 2026.10.04.
- [x] `docs/rust-migration/parity-status.md`: note the allowlisted case.

### 8. Release 2026.10.04
- [x] Separate commit `release: 2026.10.04`: set `VERSION` to `2026.10.04`, and update the
  "the release" cell in `docs/rust-migration/CONVENTIONS.md:31` and the comment in
  `Cargo.toml:7` if it names the current release rather than the baseline.

### 9. Verify
- [x] Run `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`
  and `cargo deny check`.
- [x] Run the parity harness for `09-admin`. The only diff should be the allowlisted case.
- [x] Manual check (skill `run`): start Octo against a Navidrome and open the Lyrics page.
  - Start a scan. "Getting started" shows at once, and progress and time left update.
  - Stop the server mid-scan. The page shows "Still working / Checking again", then recovers.
  - Signed out, the page asks you to sign in.

## Key Results

- **KR1:** `GET /api/admin/lyrics/library` answers 200 for a signed-in admin, with numeric
  `busy` and boolean `running`, and never leaks `foundSynced`. The 400 collision is gone.
  Covered by the ported status test.
- **KR2:** A scan with Navidrome reachable opens only songs that have lyrics (tag or sidecar)
  or that Navidrome doesn't name. This is proven by the "file does not exist" test returning a
  row.
- **KR3:** With Navidrome unreachable, or the music root impossible to list, scan results are
  identical to today's. All existing `lyrics_library_steps_tests` and `cover_upgrade_tests`
  pass unchanged.
- **KR4:** `navidrome_albums` and the lyrics scan share one Navidrome listing implementation.
  No copy of the `/api/song` paging loop is left in `cover_upgrade.rs`.
- **KR5:** `admin.js` matches upstream 2026.10.04's lyrics and cover hunks. The Lyrics page
  shows live scan progress, and it keeps polling through failed status requests after a start.
- **KR6:** `cargo fmt`, `clippy -D warnings`, `cargo test --workspace` and `cargo deny` are all
  green, and the 09-admin parity diff contains only the allowlisted, documented case.
- **KR7:** `octo_core::VERSION` reports `2026.10.04`, set in its own release commit.
