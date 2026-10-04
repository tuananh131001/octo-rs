# Test map: xUnit → cargo test

Each of the 125 C# test files in `octo.Tests/` (3,395 tests at `csharp-final`; one of them,
`LastFmRadioControllerTests.InternetRadioList_AnswersInsideTheStarterBoundAndPublishesOnTheNextRefresh`,
fails intermittently on a timeout in the baseline run) maps to a Rust location here.

| C# file | C# tests | Rust location | Rust tests | Dropped / notes |
|---|---:|---|---:|---|
| SettingsFileWriterTests.cs | 6 | `octo-core/src/settings/writer_tests.rs` | 13 | All 6 ported; 7 added: the config.md §4 golden sample byte for byte, the `fixtures/state/settings.json` round trip, case-insensitive merge, key order and wholesale replacement, `Update` writing only on true, `Load` of a corrupt file, directory creation. |
| LibraryActionSettingsTests.cs | 19 methods (38 cases) | `octo-core/src/settings/library_action_tests.rs` | 19 (+2 added) | All ported; each `[Theory]` is one test looping its cases. Added: `mapped_ratings`, `LibraryAction` index round trip. |
| RestartTrackerTests.cs | 6 | `octo-core/src/settings/restart_tracker.rs` | 6 | All ported, over a `ConfigTree` (in-memory configuration) through the `RawConfig` trait. `store_tests.rs` adds one over the live store. |
| DuplicateTests.cs (partial: `DuplicateSettingsTests` settings-only methods) | 2 methods (4 cases) | `octo-core/src/settings/library_action_tests.rs` | 2 | `Duplicates_AloneTurnsOnNoticesKeepAndTheNoticeOnlyScope`, `EffectiveDuplicatesScanInterval_IsClamped`. The rest of the file belongs to the duplicates port. |
| HeartAcquisitionCoordinatorTests.cs (partial: settings-only methods) | 2 | `octo-core/src/settings/subsonic.rs` | 2 (+2 added) | `LegacyFallbackMapsToOrderedSourcesWithLidarrLast`, `ConfiguredOrderIsPreservedAndMissingSourcesAreAppendedDisabled`. The coordinator tests belong to its port. |
| LastFmRadioCoreTests.cs (partial: settings-only methods) | 5 methods (13 cases) | `octo-core/src/settings/last_fm.rs`, `listen_brainz.rs` | 5 (+3 added) | `DiscoverySettings_NormalizeClampAndKeepStableIds`, `StationCounts_AreClampedAtReadTime`, `StationsExplicitlyEmpty_ExcusesOnlyAnAllKindsOffConfiguration`, `StreamBitrate_UsesSupportedMp3Qualities`, `ListenBrainzSettings_PickTheListenersTokenThenTheDefaultAndRespectTheSwitch`. The rest belongs to the radio port. |
| GenreNormalizerTests.cs (partial: settings-only methods) | 2 | `octo-core/src/settings/genre.rs` | 2 (+3 added) | `EffectiveMappings_KeepsTheFirstDuplicateAndTheUsersCasing`, `EffectiveBlocklist_UserEntriesAddAndCannotRemoveBuiltIns`. `BroadGenrePreset_CollapsesRealWorldJunkToAShortList` exercises the normaliser and stays with it. |
| LyricsSongSourceTests.cs (partial: settings-only methods) | 2 | `octo-core/src/settings/metadata.rs` | 2 (+3 added) | `Order_SavedWithoutTheSong_PutsItFirst`, `SaveTo_AnythingUnknown_IsBeside`. The rest belongs to the lyrics port. |
| GeneratedPlaylistTests.cs (partial: settings-only method) | 1 method (5 cases) | `octo-core/src/settings/generated_playlist.rs` | 1 (+2 added) | `Name_FollowsTheFormat_AndABrokenFormatFallsBack`; added a composite-format emulation table. The rest belongs to the mixes port. |
| ParallelDownloadTests.cs (partial: settings-only method) | 1 | `octo-core/src/settings/soulseek.rs` | 1 (+2 added) | `TheSettingIsClamped`. The rest belongs to the download port. |
| (new) settings store | — | `octo-core/src/settings/store_tests.rs` | 15 | Layering precedence, env `Section__Key` mapping, raw keys, null-in-file fallback, unconvertible values, Development layer, startup and reload parse failures, case-only duplicate keys, `for_tests` + `set`, RestartTracker over the store, `OCTO_SETTINGS_PATH`, watcher reload after a writer save, a settings directory created later. |
