// Throwaway fixture generator: builds realistic state objects with the real Octo types and
// serializes them with exactly the options each writer uses, so the fixtures are byte-exact.
using System.Globalization;
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using Octo.Models.Download;
using Octo.Models.Radio;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.LastFm;
using Octo.Services.Library;
using Octo.Services.Local;
using Octo.Services.Lyrics;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;
using Octo.Services.Tagging;
using Octo.Services.Updates;
using Octo.Services.Admin;
using Octo.Services.Fingerprint;
using Octo.Models.Settings;

var outDir = args.Length > 0 ? args[0] : "/src/out";
Directory.CreateDirectory(outDir);

DateTime U(string s) => DateTime.Parse(s, CultureInfo.InvariantCulture,
    DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal);

string P(string name)
{
    var p = Path.Combine(outDir, name);
    Directory.CreateDirectory(Path.GetDirectoryName(p)!);
    if (File.Exists(p)) File.Delete(p);
    return p;
}

// Same calls the writers make: File.WriteAllText (UTF-8, no BOM, no trailing newline).
void Write(string name, string json) => File.WriteAllText(P(name), json);

object New(Type t, params object?[] a) =>
    Activator.CreateInstance(t, BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance, null, a, null)!;

System.Collections.IList ListOf(Type t) => (System.Collections.IList)Activator.CreateInstance(typeof(List<>).MakeGenericType(t))!;

Type Nested(Type outer, string name) =>
    outer.GetNestedType(name, BindingFlags.NonPublic | BindingFlags.Public)!;

// ---------------------------------------------------------------- downloads-history.json
{
    var report = new TagReport("Strong", 0.042, "Ágætis byrjun", "4e9c2d4e-6a3b-3a3e-9a5c-1b2f7c3e9d10", "Database",
        "1999-06-12",
        [
            new TagReportCandidate("Database", "Svefn-g-englar", "Ágætis byrjun", "Album", "1999-06-12", 0.042, ["year 0.20"]),
            new TagReportCandidate("Catalog", "Svefn-g-englar", "Ágætis byrjun (Deluxe)", "Album", "2019-11-01", 0.231,
                ["album 0.45", "tracks 0.30", "year 0.20"]),
        ],
        new Dictionary<string, FieldDecision>
        {
            ["recordingId"] = new("b1a9c0e2-6f0c-4a52-8d4b-0f3c7e2d1a99", "Fingerprint"),
            ["title"] = new("Svefn-g-englar", "Database"),
            ["artist"] = new("Sigur Rós", "Database"),
            ["album"] = new("Ágætis byrjun", "Database"),
            ["track"] = new("2", "Database"),
            ["label"] = new("Smekkleysa & FatCat", "Database"),
        },
        ["the music database did not answer the release lookup; label, catalogue number and barcode may be missing"],
        new Dictionary<string, double> { ["fingerprint"] = 1.84, ["identify"] = 0.62, ["total"] = 2.5 },
        false, true, -14.21, -0.8);

    var list = new List<DownloadHistoryEntry>
    {
        new()
        {
            Artist = "Sigur Rós", Title = "Svefn-g-englar", Album = "Ágætis byrjun",
            Path = "/music/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac", Format = "FLAC", Source = "Soulseek",
            CoverArtUrl = "https://e-cdns-images.dzcdn.net/images/cover/0b5e5a0c3c1f/1000x1000-000000-80-0-0.jpg",
            SizeBytes = 62914560, TranscodedFrom = null, Tagging = report,
            DownloadedAt = U("2026-10-03T14:22:05.1234567Z").ToString("o"), RequestedBy = ["brandon", "anh"],
        },
        new()
        {
            Artist = "Queen", Title = "Don't Stop Me Now", Album = "",
            Path = "/music/Queen/Don't Stop Me Now/Don't Stop Me Now.mp3", Format = "MP3", Source = "YouTube",
            CoverArtUrl = null, SizeBytes = 8650752, TranscodedFrom = null, Tagging = null,
            DownloadedAt = U("2026-10-02T09:01:44.0807210Z").ToString("o"), RequestedBy = null,
        },
        new()
        {
            Artist = "宇多田ヒカル", Title = "First Love", Album = "First Love",
            Path = "/music/宇多田ヒカル/First Love/01 - First Love.flac", Format = "FLAC", Source = "Soulseek",
            CoverArtUrl = "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/aa/bb/cc/source/100000x100000-999.jpg",
            SizeBytes = 41203712, TranscodedFrom = "about 128 kbps MP3", Tagging = null,
            DownloadedAt = U("2026-10-01T22:10:00.5000000Z").ToString("o"), RequestedBy = ["brandon"],
        },
    };
    Write("downloads-history.json", JsonSerializer.Serialize(list));
}

// ---------------------------------------------------------------- lastfm-radio-state.json
{
    var doc = new LastFmRadioStateDocument();
    doc.Users["brandon"] = new LastFmRadioUserState
    {
        Username = "Brandon",
        LastSeenUtc = U("2026-10-03T18:40:12.3456789Z"),
        NewPlaysSinceRefresh = 3,
        Plays =
        [
            new LastFmRadioPlay
            {
                SongId = "4Kq3cS0bWq9dHq7y1nZb2e", Artist = "Björk", Title = "Jóga", Album = "Homogenic",
                Genre = "Electronic", Duration = 305, IsLocal = true, Hearted = true, LearnedSignal = true,
                Source = "scrobble", PlayedAtUtc = U("2026-10-03T18:35:00.0000000Z"),
            },
            new LastFmRadioPlay
            {
                SongId = "1aB2cD3eF4gH5iJ6kL7mN8", Artist = "Daft Punk", Title = "Digital Love", Album = null,
                Genre = null, Duration = null, IsLocal = false, Hearted = false, LearnedSignal = false,
                Source = "radio", PlayedAtUtc = U("2026-10-03T18:20:41.9876543Z"),
            },
        ],
        Stations =
        [
            new LastFmRadioStation
            {
                Id = "or3hT9kQ2mXv8LpZ1cYw", Key = "artist:björk", Name = "Björk Radio", Owner = "Brandon",
                Kind = LastFmRadioStationKind.Artist, Personalized = true, DefinitionVersion = 4,
                CreatedUtc = U("2026-09-20T07:00:00.0000000Z"), ChangedUtc = U("2026-10-03T06:00:03.1415926Z"),
                ValidUntilUtc = U("2026-10-04T06:00:03.1415926Z"), Seeds = ["Björk", "Sigur Rós"],
                Tracks =
                [
                    new LastFmRadioTrack
                    {
                        Artist = "Múm", Title = "Green Grass of Tunnel", Album = "Finally We Are No One",
                        Genre = "Electronic", Duration = 241, Year = 2002, Score = 0.8734, Source = "lastfm-similar",
                        ResolvedId = "7YzXwVu6TsRq5PoN4mLk3J", IsLocal = false, ExternalProvider = "soulseek",
                        YouTubeId = "dQw4w9WgXcQ",
                    },
                    new LastFmRadioTrack
                    {
                        Artist = "Björk", Title = "Hyperballad", Album = "Post", Genre = null, Duration = 321,
                        Year = null, Score = 1, Source = "library", ResolvedId = "9a8b7c6d5e4f3a2b1c0d9e",
                        IsLocal = true, ExternalProvider = null, YouTubeId = null,
                    },
                ],
            },
        ],
        UnavailableTracks =
        [
            new LastFmRadioUnavailableTrack
            {
                Key = "portishead|roads", Artist = "Portishead", Title = "Roads",
                FailedAtUtc = U("2026-10-03T17:00:00.0000000Z"), RetryAfterUtc = U("2026-10-04T17:00:00.0000000Z"),
            },
        ],
        LastRefreshAttemptUtc = U("2026-10-03T06:00:00.2500000Z"),
        LastRefreshSuccessUtc = U("2026-10-03T06:00:03.1415926Z"),
        LastRefreshError = null,
        Refreshing = false,
    };
    Write("lastfm-radio-state.json", JsonSerializer.Serialize(doc, new JsonSerializerOptions { WriteIndented = true }));
}

// ---------------------------------------------------------------- soulseek-holds.json
{
    var held = new List<HeldAcquisition>
    {
        new(HeldKind.Track, "soulseek", "Hq3k9ZpL2xW7mN1vB4cR8t", "brandon", U("2026-10-03T12:00:01.7654321Z")),
        new(HeldKind.Album, "soulseek", "Ab9Cd8Ef7Gh6Ij5Kl4Mn3o", null, U("2026-10-03T12:05:30.0000000Z")),
    };
    Write("soulseek-holds.json", JsonSerializer.Serialize(held));
}

// ---------------------------------------------------------------- external-ids.json (real registry)
{
    var path = P("external-ids.json");
    using (var registry = new ExternalIdRegistry(path))
    {
        registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Album, Artist = "Sigur Rós", Album = "( )", ExternalAlbumId = "302127",
        });
        registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Artist, Artist = "Simon & Garfunkel", ExternalArtistId = "1392",
        });
        var song = registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, YouTubeId = "kXYiU_JCYtU", Artist = "宇多田ヒカル", Title = "First Love",
            Album = "First Love", Duration = 257, Track = 1, DiscNumber = 1, TotalTracks = 12, Isrc = "JPTO09900010",
        });
        registry.RememberLength(song, 258, LengthSource.Deezer);
    } // Dispose flushes, exactly as on shutdown
}

// ---------------------------------------------------------------- browse-sessions.json
{
    var saved = Nested(typeof(BrowseSessionStore), "Saved");
    var list = ListOf(saved);
    list.Add(New(saved, "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08", "brandon",
        U("2027-01-01T18:40:12.3456789Z")));
    list.Add(New(saved, "60303AE22B998861BCE3B28F33EEC1BE758A213C86C93C076DBE9F558C11C752", "anh",
        U("2026-12-30T09:15:00.0012345Z")));
    Write("browse-sessions.json", JsonSerializer.Serialize(list, list.GetType()));
}

// ---------------------------------------------------------------- rejected-peers.json
{
    var entries = new List<RejectedPeerRegistry.Entry>
    {
        new("vinyl_rips_4u", @"@@abcde\Music\Björk\Homogenic\05 - Jóga.flac",
            "AcoustID thinks this is Björk - Bachelorette", "Björk - Jóga", U("2026-10-03T16:12:44.5550000Z")),
        new("peer<script>", @"@@xyz12\Shared\Queen\Don't Stop Me Now (Live).mp3",
            "reported as the wrong song by brandon", "Queen - Don't Stop Me Now", U("2026-09-28T08:00:00.0000000Z")),
    };
    Write("rejected-peers.json", JsonSerializer.Serialize(entries));
}

// ---------------------------------------------------------------- genre-backfill.json
{
    var run = new GenreBackfillRun
    {
        RunId = "20261003-142205-7f3a", Status = GenreBackfillStatus.Cancelled, Scope = GenreBackfillScope.OctoDownloads,
        DryRun = false, StartedUtc = U("2026-10-03T14:22:05.1234567Z"), FinishedUtc = U("2026-10-03T14:31:10.0000001Z"),
        Total = 4, Processed = 2, Changed = 1, Cleared = 0, Skipped = 1, Failed = 0, Cursor = 2,
        LastPath = "/music/Beyoncé/Lemonade/01 - Pray You Catch Me.flac",
        Reason = "Cancelled from the dashboard.",
        Errors = ["/music/Broken/file.mp3: Invalid header"],
        Preview =
        [
            new GenreBackfillChange("/music/Beyoncé/Lemonade/01 - Pray You Catch Me.flac", ["R&B/Soul", "Pop"], ["R&B"],
                "replace", "map:R&B/Soul->R&B"),
            new GenreBackfillChange("/music/Sigur Rós/( )/01 - Untitled #1.flac", ["Post-Rock"], ["Post-Rock"], "keep", null),
        ],
        SettingsHash = "3B1F0A9C4D2E5F60718293A4B5C6D7E8F9012345678901234567890ABCDEF12",
        Queue =
        [
            "/music/Sigur Rós/( )/01 - Untitled #1.flac",
            "/music/Beyoncé/Lemonade/01 - Pray You Catch Me.flac",
            "/music/宇多田ヒカル/First Love/01 - First Love.flac",
            "/music/Queen/Jazz/12 - Don't Stop Me Now.flac",
        ],
    };
    Write("genre-backfill.json", JsonSerializer.Serialize(run));
}

// ---------------------------------------------------------------- genre-backfill-journal.jsonl
{
    var path = P("genre-backfill-journal.jsonl");
    var journal = new GenreBackfillJournal(path);
    journal.Append(new GenreJournalEntry("/music/Beyoncé/Lemonade/01 - Pray You Catch Me.flac", ["R&B/Soul", "Pop"], ["R&B"],
        U("2026-10-03T14:25:00.1000000Z"), "20261003-142205-7f3a"));
    journal.Append(new GenreJournalEntry("/music/Sigur Rós/( )/02 - Untitled #2.flac", [], ["Post-Rock"],
        U("2026-10-03T14:25:01.2000000Z"), "20261003-142205-7f3a"));
}

// ---------------------------------------------------------------- cover-upgrade.json
{
    var run = new CoverUpgradeRun
    {
        RunId = "cu-20261003-0815", Status = CoverUpgradeStatus.Completed, Scope = CoverUpgradeScope.WholeLibrary,
        Mode = CoverUpgradeMode.Preview, FolderCovers = true, SmallerThan = 1000,
        Selected = ["a1b2c3d4e5f60718", "0f1e2d3c4b5a6978"], FullSize = false, Undo = false,
        StartedUtc = U("2026-10-03T08:15:00.0000000Z"), FinishedUtc = U("2026-10-03T08:19:42.4242424Z"),
        Total = 2, Processed = 2, SongsTotal = 23, SongsRead = 23, AlbumsTotal = 2, AlbumsDone = 2,
        Soft = 2, Upgraded = 1, Kept = 1, Files = 11, Failed = 0, Cursor = 2,
        LastFolder = "/music/Mötley Crüe/Dr. Feelgood", Reason = null, Errors = [],
        Preview =
        [
            new CoverUpgradeChange("a1b2c3d4e5f60718", "/music/Mötley Crüe/Dr. Feelgood", "Mötley Crüe", "Dr. Feelgood",
                500, 3000, "itunes", 11, true, "found", "/music/Mötley Crüe/Dr. Feelgood/01 - T.n.T. (Terror 'n Tinseltown).flac",
                null, "al-4f3e2d1c", "075596084322", false),
            new CoverUpgradeChange("0f1e2d3c4b5a6978", "/music/Singles", "Simon & Garfunkel", null, 600, 600, null, 1, false,
                "none", null, ["/music/Singles/Simon & Garfunkel - The Boxer.mp3"]),
        ],
        Queue =
        [
            new CoverUpgradeItem("/music/Mötley Crüe/Dr. Feelgood", null, "al-4f3e2d1c"),
            new CoverUpgradeItem("/music/Singles", ["/music/Singles/Simon & Garfunkel - The Boxer.mp3"]),
        ],
    };
    Write("cover-upgrade.json", JsonSerializer.Serialize(run));
}

// ---------------------------------------------------------------- cover-upgrade-journal.jsonl
{
    var path = P("cover-upgrade-journal.jsonl");
    // Same line shape as CoverUpgradeJournal.Record, written the same way (AppendAllText + NewLine).
    foreach (var e in new[]
             {
                 new CoverUpgradeJournal.Entry("/music/Mötley Crüe/Dr. Feelgood/01 - T.n.T. (Terror 'n Tinseltown).flac",
                     CoverUpgradeJournal.Embedded, "5d41402abc4b2a76b9719d911017c592", "cu-20261003-0900"),
                 new CoverUpgradeJournal.Entry("/music/Mötley Crüe/Dr. Feelgood/cover.jpg", CoverUpgradeJournal.FolderFile,
                     null, "cu-20261003-0900"),
             })
        File.AppendAllText(path, JsonSerializer.Serialize(e) + Environment.NewLine);
}

// ---------------------------------------------------------------- library-actions.json
{
    var entries = new List<LibraryActionEntry>
    {
        new("Delete|nd-8f7e6d5c|62914560:639323617251234567", LibraryAction.Delete, "nd-8f7e6d5c", "brandon",
            "Svefn-g-englar", "Sigur Rós", "Ágætis byrjun", "/music/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac",
            "/music/.octo-trash/2026-10-03/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac", PathSource.NativeApi,
            LibraryActionState.Applied, null, false, U("2026-10-03T15:00:00.1234567Z")),
        new("BetterQuality|nd-1a2b3c4d|8650752:639322000000000000", LibraryAction.BetterQuality, "nd-1a2b3c4d", "anh",
            "Don't Stop Me Now", "Queen", "Jazz", "/music/Queen/Jazz/12 - Don't Stop Me Now.mp3", null, PathSource.LocalMappings,
            LibraryActionState.Pending, "Looking for a lossless copy <FLAC> & checking it", false, U("2026-10-03T15:05:00.0000000Z"))
        {
            HistoryKept = true, RevealedPath = null,
        },
    };
    Write("library-actions.json", JsonSerializer.Serialize(entries));
}

// ---------------------------------------------------------------- notice-queue.json
{
    var json = new JsonSerializerOptions { Converters = { new JsonStringEnumConverter() } };
    var entries = new List<NoticeEntry>
    {
        new()
        {
            Key = "review|brandon|/music/Björk/Homogenic/05 - Jóga.flac", Kind = NoticeKind.Review, Username = "brandon",
            LocalPath = "/music/Björk/Homogenic/05 - Jóga.flac", Artist = "Björk", Title = "Jóga", Album = "Homogenic",
            NavidromeId = "nd-77aa88bb", GroupKey = null, Order = 0, State = NoticeState.Waiting,
            Reason = "AcoustID thinks this is Björk - \"Bachelorette\"", Cause = InconclusiveReason.SourceDisagreed,
            Origin = NoticeOrigin.Download, Fingerprint = "AQADtEmUaEkSRZEGAAAAAAAA", DurationSeconds = 305,
            CandidateRecordingId = "c0ffee00-1234-5678-9abc-def012345678", FileFormat = "flac", Submitted = false,
            LookupAttempts = 1, NextLookupUtc = U("2026-10-04T15:00:00.0000000Z"), CreatedUtc = U("2026-10-03T15:00:00.9876543Z"),
            QueuedUtc = null, ResolvedUtc = null,
        },
        new()
        {
            Key = "duplicates|anh|queen|don't stop me now", Kind = NoticeKind.Duplicates, Username = "anh",
            LocalPath = "/music/Queen/Jazz/12 - Don't Stop Me Now.flac", Artist = "Queen", Title = "Don't Stop Me Now",
            Album = "Jazz", NavidromeId = "nd-1a2b3c4d", GroupKey = "queen|don't stop me now", Order = 1,
            State = NoticeState.Kept, Reason = "Two copies of this song", Cause = InconclusiveReason.None,
            Origin = NoticeOrigin.LibrarySweep, Fingerprint = null, DurationSeconds = 209, CandidateRecordingId = null,
            FileFormat = "flac", Submitted = false, LookupAttempts = 0, NextLookupUtc = DateTime.MinValue,
            CreatedUtc = U("2026-10-01T10:00:00.0000000Z"), QueuedUtc = U("2026-10-01T10:00:05.0000000Z"),
            ResolvedUtc = U("2026-10-02T08:30:00.0000000Z"),
        },
    };
    Write("notice-queue.json", JsonSerializer.Serialize(entries, json));
}

// ---------------------------------------------------------------- generated-playlists.json
{
    var stateDoc = Nested(typeof(GeneratedPlaylistService), "StateDocument");
    var doc = New(stateDoc);
    var users = new Dictionary<string, GeneratedPlaylistService.UserMixes>(StringComparer.Ordinal)
    {
        ["brandon"] = new()
        {
            Active = ["genre:Rock", "genre:Électronique", "decade:1990"],
            Counts = new(StringComparer.Ordinal)
            {
                ["genre:Rock"] = 412, ["genre:Électronique"] = 57, ["genre:R&B"] = 12, ["decade:1990"] = 233, ["decade:2020"] = 0,
            },
            CountsUtc = U("2026-10-03T06:00:00.5555555Z"),
            Kinds = "genre,decade",
        },
    };
    stateDoc.GetProperty("Users")!.SetValue(doc, users);
    Write("generated-playlists.json", JsonSerializer.Serialize(doc, stateDoc, new JsonSerializerOptions { WriteIndented = false }));
}

// ---------------------------------------------------------------- quality-upgrade.json
{
    var state = new QualityUpgradeState
    {
        LastRunUtc = U("2026-10-03T03:00:00.0000000Z"),
        LastOutcome = "Applied",
        Attempts = new()
        {
            ["Queen/Jazz/12 - Don't Stop Me Now.mp3|8650752"] = new(U("2026-10-03T03:00:00.0000000Z"), "Applied",
                "Replaced with a FLAC (24-bit/96 kHz)"),
            ["宇多田ヒカル/First Love/01 - First Love.m4a|9123456"] = new(U("2026-09-26T03:00:00.0000000Z"), "Failed",
                "No source had a lossless copy"),
        },
    };
    Write("quality-upgrade.json", JsonSerializer.Serialize(state));
}

// ---------------------------------------------------------------- upgrades.json
{
    var jobs = new List<UpgradeJob>
    {
        new()
        {
            NavidromeId = "nd-1a2b3c4d", Title = "Don't Stop Me Now", Artist = "Queen", Album = "Jazz", Suffix = "mp3",
            AttemptKey = "Queen/Jazz/12 - Don't Stop Me Now.mp3|8650752", RequestedBy = "brandon", Origin = "app",
            State = UpgradeStates.Upgraded, Detail = "Replaced with a FLAC", AcquisitionKey = "soulseek:Hq3k9ZpL2xW7mN1vB4cR8t",
            QueuedUtc = U("2026-10-03T10:00:00.0000000Z"), UpdatedUtc = U("2026-10-03T10:04:31.2500000Z"),
            StartedUtc = U("2026-10-03T10:00:02.0000000Z"),
            Result = new UpgradeResult
            {
                Before = "MP3 320 kbps", BeforeBytes = 8650752, After = "FLAC 16-bit/44.1 kHz", AfterBytes = 31457280,
                NewFile = "12 - Don't Stop Me Now.flac", KeptAt = ".octo-trash/2026-10-03",
                Checks = ["the same length", "AcoustID: the same recording", "the spectrum: really lossless, not a converted MP3"],
                Seconds = 269,
            },
        },
        new()
        {
            NavidromeId = "nd-99ee88dd", Title = "Jóga", Artist = "Björk", Album = "Homogenic", Suffix = "m4a",
            AttemptKey = null, RequestedBy = "anh", Origin = "page", State = UpgradeStates.Queued, Detail = null,
            AcquisitionKey = null, QueuedUtc = U("2026-10-03T11:00:00.1000000Z"), UpdatedUtc = U("2026-10-03T11:00:00.1000000Z"),
            StartedUtc = null, Result = null,
        },
    };
    Write("upgrades.json", JsonSerializer.Serialize(jobs));
}

// ---------------------------------------------------------------- update/release.json
{
    var state = new ReleaseCheckState
    {
        Repo = "winters27/octo", CheckedUtc = U("2026-10-03T18:00:00.1234567Z"), AttemptedUtc = U("2026-10-03T18:00:00.1234567Z"),
        Error = null, ETag = "W/\"5f2c8a0e1b\"",
        Releases =
        [
            new ReleaseNote("2026.10.03.2", "2026.10.03.2", "### Lyrics\n- A Lyrics page that scans for songs needing better lyrics.\n- Fixed <br> & quotes in notes.",
                "https://github.com/winters27/octo/releases/tag/2026.10.03.2", U("2026-10-03T16:45:10Z")),
            new ReleaseNote("2026.10.03.1", "2026.10.03.1", "Albums are filled in only from an album that holds its songs.",
                "https://github.com/winters27/octo/releases/tag/2026.10.03.1", null),
        ],
    };
    Write("update/release.json", JsonSerializer.Serialize(state));
}

// ---------------------------------------------------------------- review-sweep.json
{
    var state = new ReviewSweepState
    {
        Paused = false, Cursor = "Björk/Homogenic/05 - Jóga.flac",
        Checked = new(StringComparer.Ordinal)
        {
            ["Björk/Homogenic/04 - Bachelorette.flac"] = "41203712:639322987654321000",
            ["Queen/Jazz/12 - Don't Stop Me Now.flac"] = "31457280:639323000000000000",
        },
        Pass = 2, Total = 1874, Found = 3, Fine = 1720, Undecodable = 1,
        LastCheckedUtc = U("2026-10-03T17:59:00.0000000Z"), PassFinishedUtc = U("2026-09-26T04:12:00.0000000Z"),
        NextPassUtc = U("2026-10-10T04:12:00.0000000Z"),
    };
    Write("review-sweep.json", JsonSerializer.Serialize(state));
}

// ---------------------------------------------------------------- itunes-masters.json
{
    var row = Nested(typeof(ITunesCoverArtLookup), "CachedMaster");
    var rows = ListOf(row);
    rows.Add(New(row, SongIdentity.MatchKey("Mötley Crüe", "Dr. Feelgood") + "|album",
        "https://is1-ssl.mzstatic.com/image/thumb/Music115/v4/12/34/56/source/100000x100000-999.jpg",
        U("2026-10-03T08:16:00.0000000Z")));
    rows.Add(New(row, SongIdentity.MatchKey("Simon & Garfunkel", "The Boxer") + "|single", null,
        U("2026-10-03T08:17:30.0000000Z")));
    Write("itunes-masters.json", JsonSerializer.Serialize(rows, rows.GetType()));
}

// ---------------------------------------------------------------- lyrics-choices.json
{
    var pins = new List<LyricsPin>
    {
        new("nd-77aa88bb", "lrclib:123456", "lrclib",
            "[00:12.34]Emotional landscapes\n[00:18.90]They puzzle me", null, "Björk", "Jóga", "brandon",
            U("2026-10-03T12:00:00.0000000Z")),
        new("ext-9a8b7c6d5e4f3a2b1c0d9e", LyricsPin.Hidden, null, null, null, "Portishead", "Roads", "anh",
            U("2026-10-02T21:30:00.1200000Z")),
    };
    Write("lyrics-choices.json", JsonSerializer.Serialize(pins));
}

// ---------------------------------------------------------------- lyrics-library.json
{
    var run = new LyricsLibraryRun
    {
        RunId = "ly-20261003-2000", Status = LyricsLibraryStatus.Completed, Scope = "WholeLibrary", Upgrade = true,
        Mode = LyricsLibraryMode.Preview,
        Rows =
        [
            new LyricsLibraryRow
            {
                Id = LyricsLibraryRow.IdOf("/music/宇多田ヒカル/First Love/01 - First Love.flac"),
                Path = "/music/宇多田ヒカル/First Love/01 - First Love.flac", Artist = "宇多田ヒカル", Title = "First Love",
                Album = "First Love", Has = "plain", Result = "found", Source = "kugou", Kind = "word",
                CandidateId = "kugou:abc123", Doubt = null, Preview = ["最後のキスは", "タバコのflavorがした"],
                FoundSynced = "[00:15.20]<00:15.20>最後の<00:16.10>キスは\n[00:19.80]タバコのflavorがした", FoundPlain = null,
            },
            new LyricsLibraryRow
            {
                Id = LyricsLibraryRow.IdOf("/music/Queen/Jazz/12 - Don't Stop Me Now.flac"),
                Path = "/music/Queen/Jazz/12 - Don't Stop Me Now.flac", Artist = "Queen", Title = "Don't Stop Me Now",
                Album = "Jazz", Has = "none", Result = "weak",
            },
        ],
        Picked = null, WordAlready = 4,
        StartedUtc = U("2026-10-03T20:00:00.0000000Z"), FinishedUtc = U("2026-10-03T20:03:12.7000000Z"),
        Total = 2, Processed = 2, Written = 0, WordTimed = 1, Upgraded = 0, AlreadyHad = 0, NotFound = 1, Instrumental = 0,
        Busy = 0, Skipped = 0, Failed = 0, Cursor = 2, LastPath = "/music/Queen/Jazz/12 - Don't Stop Me Now.flac",
        Reason = null, Errors = [],
        Queue = ["/music/宇多田ヒカル/First Love/01 - First Love.flac", "/music/Queen/Jazz/12 - Don't Stop Me Now.flac"],
        Review =
        [
            new LyricsReviewEntry("/music/宇多田ヒカル/First Love/01 - First Love.flac", "宇多田ヒカル", "First Love", "First Love",
                257, "kugou", "word", "kugou:abc123", "the length could not be checked", U("2026-10-03T20:01:00.0000000Z")),
        ],
    };
    Write("lyrics-library.json", JsonSerializer.Serialize(run));
}

// ---------------------------------------------------------------- lyrics-undo.jsonl
{
    var path = P("lyrics-undo.jsonl");
    // LyricsUndoJournal.Record stamps DateTime.UtcNow; the line shape is reproduced with fixed times.
    foreach (var e in new[]
             {
                 new LyricsUndoJournal.Entry("/music/宇多田ヒカル/First Love/01 - First Love.lrc", LyricsUndoJournal.Beside, null,
                     "ly-20261003-2005", U("2026-10-03T20:05:00.1000000Z")),
                 new LyricsUndoJournal.Entry("/music/Queen/Jazz/12 - Don't Stop Me Now.flac", LyricsUndoJournal.Inside,
                     "Tonight I'm gonna have myself\nA real good time", "ly-20261003-2005", U("2026-10-03T20:05:01.2000000Z")),
             })
        File.AppendAllText(path, JsonSerializer.Serialize(e) + "\n");
}

// ---------------------------------------------------------------- <download dir>/.mappings.json
{
    var mappings = new Dictionary<string, LocalSongMapping>
    {
        ["soulseek:7YzXwVu6TsRq5PoN4mLk3J"] = new()
        {
            ExternalProvider = "soulseek", ExternalId = "7YzXwVu6TsRq5PoN4mLk3J",
            LocalPath = "/music/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac", LocalSubsonicId = null,
            Title = "Svefn-g-englar", Artist = "Sigur Rós", Album = "Ágætis byrjun",
            DownloadedAt = U("2026-10-03T14:22:05.1234567Z"), SourcePeer = "vinyl_rips_4u",
            SourceFile = @"@@abcde\Music\Sigur Rós\Ágætis byrjun\02 Svefn-g-englar.flac",
            MusicBrainzRecordingId = "b1a9c0e2-6f0c-4a52-8d4b-0f3c7e2d1a99", TranscodedFrom = null,
        },
        ["youtube:dQw4w9WgXcQ"] = new()
        {
            ExternalProvider = "youtube", ExternalId = "dQw4w9WgXcQ",
            LocalPath = "/music/Queen/Don't Stop Me Now/Don't Stop Me Now.mp3", Title = "Don't Stop Me Now",
            Artist = "Queen", Album = "", DownloadedAt = U("2026-10-02T09:01:44.0807210Z"),
        },
    };
    Write("music/.mappings.json", JsonSerializer.Serialize(mappings, new JsonSerializerOptions { WriteIndented = true }));
}

// ---------------------------------------------------------------- quarantine manifest
{
    var manifest = new QuarantineManifest("/music/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac", "nd-8f7e6d5c",
        LibraryAction.Delete.ToString(), "brandon", U("2026-10-03T15:00:00.2345678Z"));
    Write("music/.octo-trash/2026-10-03/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac.octo-action.json",
        JsonSerializer.Serialize(manifest));
}

// ---------------------------------------------------------------- radio cache profile sidecar
{
    var profile = new RadioAudioProfile(-13.42, 6.1, -0.9, -0.58, 2214.37, 0.1834, 7811.5, "Electronic", ["electronic", "trip-hop", "Björk"]);
    Write("cache/radio/3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca046dea.mp3.json",
        JsonSerializer.Serialize(profile));
}

// ---------------------------------------------------------------- settings.json (via the real writer)
{
    var path = P("settings.json");
    var writer = new SettingsFileWriter(path);
    writer.Merge(JsonNode.Parse("""
        {
          "Subsonic": { "Url": "http://navidrome:4533", "AdminUsername": "brandon", "AutoDetectDownloadPath": true },
          "LastFm": { "EnableRadio": true, "RadioTrackCount": 50 },
          "Library": { "DownloadPath": "/music" }
        }
        """)!.AsObject());
    // What Connect does: a session under LastFm.UserSessions (LastFmScrobbleService.SessionsIn).
    writer.Update(root =>
    {
        var sessions = new JsonObject { ["SessionKey"] = "a1b2c3d4e5f6", ["LastFmUser"] = "Brandön & co" };
        ((JsonObject)root["LastFm"]!)["UserSessions"] = new JsonObject { ["brandon"] = sessions };
        return true;
    });
}

// ---------------------------------------------------------------- update/request (key=value, not JSON)
{
    var host = new UpdateHost(Path.Combine(outDir, "update"),
        Microsoft.Extensions.Logging.Abstractions.NullLogger<UpdateHost>.Instance,
        () => U("2026-10-03T18:05:00Z"));
    host.Request("2026.10.03.2", "brandon");
}

Console.WriteLine("done");
