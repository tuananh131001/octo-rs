// Reference generator for the Rust tag port (task 3-D): writes tags onto the tiny ffmpeg files in
// ../input with the real C# code (TagLibSharp 2.3.0, TagWriterExtras, KeptIdentityTags, and a copy
// of the tag-writing body of BaseDownloadService.WriteMetadataAsync), then dumps every frame,
// field and atom of each result as TagLib reads it, plus what Octo's own readers make of it.
// The Rust test (crates/octo-media/src/tags/fixture_tests.rs) runs the same scenarios with the
// Rust writer and compares its dump of its own files, and of these files, with these dumps.
//
// Run ../generate.sh from anywhere; it builds this against octo/octo.csproj as octo.Tests (for
// the internal types) in the .NET 9 SDK image, with no LANG set, as the shipped image runs (the
// current culture is the invariant one, so a new USLT frame's language is "ivl").
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Library;

var root = args.Length > 0 ? args[0] : "/repo/docs/rust-migration/fixtures/tags";
var input = Path.Combine(root, "input");
var output = Path.Combine(root, "csharp");
Directory.CreateDirectory(output);
foreach (var old in Directory.GetFiles(output)) File.Delete(old);

var cover = File.ReadAllBytes(Path.Combine(input, "cover.png"));
string[] formats = ["mp3", "flac", "m4a", "opus"];
var written = new List<string>();

string Copy(string format, string scenario)
{
    var path = Path.Combine(output, $"{scenario}.{format}");
    File.Copy(Path.Combine(input, $"input.{format}"), path, overwrite: true);
    written.Add(path);
    return path;
}

string CopyOf(string from, string scenario)
{
    var path = Path.Combine(output, scenario + Path.GetExtension(from));
    File.Copy(from, path, overwrite: true);
    written.Add(path);
    return path;
}

foreach (var format in formats)
{
    // full: every field the download pipeline writes, with a cover.
    var full = Copy(format, "full");
    Scenarios.WriteSong(full, Scenarios.FullSong(), Scenarios.Genres.Write, ["Trip Hop", "Rock"], cover);

    // minimal: a title and an artist; the genre feature off. On an MP3 none of TagWriterExtras
    // runs, so the new ID3 tag keeps TagLib's default version, 3.
    var minimal = Copy(format, "minimal");
    Scenarios.WriteSong(minimal, new Song { Title = "Song", Artist = "Artist", Genre = "Pop" },
        Scenarios.Genres.FeatureOff, null, null);

    // peer: a file that arrived tagged (version 3 on an MP3), then Octo's extras on top.
    var peer = Copy(format, "peer");
    Scenarios.Peer(peer, cover);

    // lyrics: LyricsSidecarWriter.WriteInside, then its undo with nothing before.
    var lyrics = Copy(format, "lyrics");
    Scenarios.SetLyrics(lyrics, LyricsMark() + "\n" + "[00:01.00]Ça va — 東京\n[00:02.50]second line");
    var unlyrics = CopyOf(lyrics, "unlyrics");
    Scenarios.SetLyrics(unlyrics, null);

    // albumgain: WriteAlbumGain rewriting a tagged file in place.
    var albumGain = CopyOf(full, "albumgain");
    Scenarios.AlbumGain(albumGain);

    // cleared: the genre plan's Clear, a compilation flag set false, the pictures dropped.
    var cleared = CopyOf(full, "cleared");
    Scenarios.Clear(cleared);

    // keptsrc + kept: KeptIdentityTags.Read from an original, Apply onto a fully tagged file.
    var keptSource = Copy(format, "keptsrc");
    Scenarios.KeptSource(keptSource, format);
    var kept = CopyOf(full, "kept");
    KeptIdentityTags.Apply(kept, KeptIdentityTags.Read(keptSource)!);

    // exact: SetExact replacing and removing, SetMultiValue with blanks, SetText trimming.
    var exact = CopyOf(full, "exact");
    Scenarios.Exact(exact);
}

// One dump per file, and TagLib's own genre table, which the numeric genre frames index.
foreach (var path in written)
    File.WriteAllText(path + ".dump", Dumper.Dump(path), new UTF8Encoding(false));
File.WriteAllText(Path.Combine(output, "genres.txt"), string.Join("\n", TagLib.Genres.Audio) + "\n",
    new UTF8Encoding(false));
Console.WriteLine($"culture '{CultureInfo.CurrentCulture.Name}' ({CultureInfo.CurrentCulture.ThreeLetterISOLanguageName}); {written.Count} files");

static string LyricsMark() => Octo.Services.Lyrics.LyricsSidecarWriter.OctoMark;

static class Scenarios
{
    public enum Genres { FeatureOff, Write, Clear, None }

    public static Song FullSong() => new()
    {
        Title = "Teardrop",
        Artist = "Massive Attack feat. Elizabeth Fraser",
        Artists = ["Massive Attack", "Elizabeth Fraser"],
        PrimaryArtist = "Massive Attack",
        Album = "Mezzanine",
        AlbumArtist = "Massive Attack",
        Track = 3,
        TotalTracks = 11,
        DiscNumber = 1,
        Year = 1998,
        Bpm = 77,
        Contributors = ["Robert Del Naja", "Grant Marshall"],
        Copyright = "℗ 1998 Virgin Records",
        MusicBrainzRecordingId = "5b0ef8e9-9b55-4a3b-9a6b-3a2f6b1c7d01",
        MusicBrainzReleaseGroupId = "b2b4e1d0-6f5e-4a7c-8d9e-0f1a2b3c4d5e",
        MusicBrainzAlbumTitle = "Mezzanine",
        MusicBrainzArtistIds = ["10adbe5e-a2c0-4bf3-8249-2b4cbf6e6ca8", "2f5e3d3a-0000-4000-8000-000000000001"],
        IsCompilation = true,
        Isrc = "gb-aaa-98-00001",
        Label = "Virgin",
        CatalogNumber = "CDV 2851",
        Barcode = "724384559922",
        ReleaseType = "album; compilation",
        ReleaseStatus = "official",
        ReleaseCountry = "GB",
        OriginalDate = "1998-04-20",
        MusicBrainzReleaseTrackId = "8e7d6c5b-4a3f-4d2c-9b1a-0e9f8d7c6b5a",
        MusicBrainzAlbumArtistIds = ["10adbe5e-a2c0-4bf3-8249-2b4cbf6e6ca8"],
        AcoustId = "acoustid-1",
        ReplayGainTrackGainDb = -6.52,
        ReplayGainTrackPeak = 0.891251,
        ReplayGainAlbumGainDb = 3.1,
        ReplayGainAlbumPeak = 1.0,
    };

    /// <summary>The tag-writing body of BaseDownloadService.WriteMetadataAsync, copied as it is
    /// at csharp-final. The genre plan and the cover chain are its callers' decisions: the
    /// plan's action and genres, and the bytes to embed, come in as arguments.</summary>
    public static void WriteSong(string filePath, Song song, Genres genreAction, string[]? planGenres, byte[]? embed)
    {
        using var tagFile = TagLib.File.Create(filePath);

        if (!string.IsNullOrEmpty(song.Title)) tagFile.Tag.Title = song.Title;
        if (!string.IsNullOrEmpty(song.Artist)) tagFile.Tag.Performers = new[] { song.Artist };
        if (song.Artists.Count > 1) TagWriterExtras.SetMultiValue(tagFile, "ARTISTS", song.Artists);
        if (!string.IsNullOrEmpty(song.Album)) tagFile.Tag.Album = song.Album;
        if (!string.IsNullOrEmpty(song.AlbumArtist))
            tagFile.Tag.AlbumArtists = new[] { song.AlbumArtist };
        else if (!string.IsNullOrEmpty(song.Artist))
            tagFile.Tag.AlbumArtists = new[] { song.PrimaryArtist ?? song.Artist };

        if (song.Track is > 0)
        {
            tagFile.Tag.Track = (uint)song.Track.Value;
            if (song.TotalTracks.HasValue)
                tagFile.Tag.TrackCount = (uint)song.TotalTracks.Value;
        }

        if (song.DiscNumber.HasValue)
            tagFile.Tag.Disc = (uint)song.DiscNumber.Value;

        if (song.Year.HasValue)
            tagFile.Tag.Year = (uint)song.Year.Value;

        switch (genreAction)
        {
            case Genres.Write: tagFile.Tag.Genres = planGenres!.ToArray(); break;
            case Genres.Clear: tagFile.Tag.Genres = []; break;
            case Genres.FeatureOff when !string.IsNullOrEmpty(song.Genre): tagFile.Tag.Genres = new[] { song.Genre }; break;
        }

        if (song.Bpm.HasValue)
            tagFile.Tag.BeatsPerMinute = (uint)song.Bpm.Value;

        if (song.Contributors.Count > 0)
            tagFile.Tag.Composers = song.Contributors.ToArray();

        if (!string.IsNullOrEmpty(song.Copyright))
            tagFile.Tag.Copyright = song.Copyright;

        if (!string.IsNullOrEmpty(song.MusicBrainzRecordingId))
            TagWriterExtras.SetRecordingId(tagFile, song.MusicBrainzRecordingId);
        var albumIsRelease = !string.IsNullOrEmpty(song.MusicBrainzReleaseGroupId)
            && Octo.Services.Fingerprint.VerificationResult.AlbumIsFromRelease(song);
        if (albumIsRelease) tagFile.Tag.MusicBrainzReleaseGroupId = song.MusicBrainzReleaseGroupId;
        if (song.MusicBrainzArtistIds.Count > 0) TagWriterExtras.SetMulti(tagFile, TagFields.ArtistId, song.MusicBrainzArtistIds);
        if (song.IsCompilation) TagWriterExtras.SetCompilation(tagFile, true);
        else if (song.TagPlan is { AlbumFromCandidate: true, Rehearsed: false }) TagWriterExtras.SetCompilation(tagFile, false);

        TagWriterExtras.SetText(tagFile, TagFields.Isrc, SongIdentity.NormalizeIsrc(song.Isrc));
        TagWriterExtras.SetText(tagFile, TagFields.Label, song.Label);
        TagWriterExtras.SetText(tagFile, TagFields.CatalogNumber, song.CatalogNumber);
        TagWriterExtras.SetText(tagFile, TagFields.Barcode, song.Barcode);
        if (song.ReleaseType is { Length: > 0 } releaseType)
            TagWriterExtras.SetMulti(tagFile, TagFields.ReleaseType, releaseType.Split("; ", StringSplitOptions.RemoveEmptyEntries));
        TagWriterExtras.SetText(tagFile, TagFields.ReleaseStatus, song.ReleaseStatus);
        TagWriterExtras.SetText(tagFile, TagFields.ReleaseCountry, song.ReleaseCountry);
        TagWriterExtras.SetOriginalDate(tagFile, song.OriginalDate);
        if (albumIsRelease) TagWriterExtras.SetReleaseTrackId(tagFile, song.MusicBrainzReleaseTrackId);
        if (albumIsRelease) TagWriterExtras.SetMulti(tagFile, TagFields.AlbumArtistId, song.MusicBrainzAlbumArtistIds);
        TagWriterExtras.SetText(tagFile, TagFields.FingerprintId, song.AcoustId);
        TagWriterExtras.SetReplayGain(tagFile, song.ReplayGainTrackGainDb, song.ReplayGainTrackPeak,
            song.ReplayGainAlbumGainDb, song.ReplayGainAlbumPeak);

        if (embed is not null)
        {
            tagFile.Tag.Pictures = new TagLib.IPicture[]
            {
                new TagLib.Picture
                {
                    Type = TagLib.PictureType.FrontCover,
                    MimeType = Octo.Services.CoverArt.CoverImage.MimeType(embed),
                    Description = "Cover",
                    Data = new TagLib.ByteVector(embed),
                },
            };
        }

        tagFile.Save();
    }

    /// <summary>A peer's file: on an MP3 a version 3 tag with a slash in the album artist and a
    /// non-Latin title; then the extras Octo adds to a file it keeps.</summary>
    public static void Peer(string path, byte[] cover)
    {
        using (var file = TagLib.File.Create(path))
        {
            if (file is TagLib.Mpeg.AudioFile)
            {
                var id3 = (TagLib.Id3v2.Tag)file.GetTag(TagLib.TagTypes.Id3v2, true);
                id3.Version = 3;
                id3.SetTextFrame("TPE2", "AC/DC");
            }
            else
            {
                file.Tag.AlbumArtists = ["AC/DC"];
            }
            file.Tag.Title = "Ágætis byrjun — 東京";
            file.Tag.Performers = ["Sigur Rós"];
            file.Tag.Genres = ["Rock", "Shoegaze"];
            file.Tag.Track = 7;
            file.Tag.Disc = 2;
            file.Tag.DiscCount = 2;
            file.Tag.Year = 1999;
            file.Save();
        }

        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetOriginalDate(file, "1999-06-12");
            TagWriterExtras.SetText(file, TagFields.Label, "  Smekkleysa Ünïcode  ");
            TagWriterExtras.SetMulti(file, TagFields.ArtistId, ["a-1", "a-2", "a-1", " "]);
            TagWriterExtras.SetMultiValue(file, "ARTISTS", ["Sigur Rós", "坂本龍一"]);
            TagWriterExtras.SetRecordingId(file, "rec-peer");
            TagWriterExtras.SetCompilation(file, false);
            file.Tag.Lyrics = "plain peer lyrics\nwith ünïcode";
            file.Tag.Pictures = new TagLib.IPicture[]
            {
                new TagLib.Picture
                {
                    Type = TagLib.PictureType.FrontCover, MimeType = "image/png", Description = "Cover",
                    Data = new TagLib.ByteVector(cover),
                },
            };
            file.Save();
        }
    }

    public static void SetLyrics(string path, string? lyrics)
    {
        using var file = TagLib.File.Create(path);
        file.Tag.Lyrics = lyrics;
        file.Save();
    }

    public static void AlbumGain(string path)
    {
        using var file = TagLib.File.Create(path);
        TagWriterExtras.SetReplayGain(file, null, null, -7.25, 0.98765432);
        file.Save();
    }

    public static void Clear(string path)
    {
        using var file = TagLib.File.Create(path);
        file.Tag.Genres = [];
        TagWriterExtras.SetCompilation(file, false);
        file.Tag.Pictures = [];
        file.Save();
    }

    /// <summary>The original a replacement takes the place of (KeptIdentityTests' shape).</summary>
    public static void KeptSource(string path, string format)
    {
        using var file = TagLib.File.Create(path);
        file.Tag.Title = "Teardrop"; file.Tag.Album = "Mezzanine"; file.Tag.AlbumArtists = ["Massive Attack", "Tracey Thorn"];
        file.Tag.Track = 3; file.Tag.TrackCount = 11; file.Tag.Disc = 1; file.Tag.DiscCount = 2;
        if (format == "mp3")
            ((TagLib.Id3v2.Tag)file.GetTag(TagLib.TagTypes.Id3v2, true)).SetTextFrame("TDRL", "1998-04-20");
        else
            TagWriterExtras.SetText(file, TagFields.ReleaseDate, "1998-04-20");
        TagWriterExtras.SetText(file, TagFields.AlbumVersion, "Original");
        TagWriterExtras.SetText(file, TagFields.AlbumId, "1D2B6C3E-7A4F-4E1B-9C2D-3F4A5B6C7D8E");
        TagWriterExtras.SetReleaseTrackId(file, "{8E7D6C5B-4A3F-4D2C-9B1A-0E9F8D7C6B5A}");
        TagWriterExtras.SetExact(file, TagFields.AlbumArtists, ["Massive Attack", "Tracey Thorn"]);
        file.Save();
    }

    public static void Exact(string path)
    {
        using var file = TagLib.File.Create(path);
        TagWriterExtras.SetExact(file, TagFields.AlbumArtists, [" Massive Attack ", "", "Tracey Thorn"]);
        TagWriterExtras.SetExact(file, TagFields.AlbumVersion, ["Deluxe"]);
        TagWriterExtras.SetExact(file, TagFields.AlbumVersion, []);
        TagWriterExtras.SetExact(file, TagFields.Barcode, []);
        TagWriterExtras.SetMultiValue(file, "ARTISTS", [" A ", "", "B"]);
        TagWriterExtras.SetText(file, TagFields.Label, "  Trimmed  ");
        TagWriterExtras.SetMulti(file, TagFields.Isrc, ["GBAAA9800001", "GBAAA9800002"]);
        file.Save();
    }
}

/// <summary>Every frame, field and atom as TagLib reads them back, then Octo's readers' views.
/// One line each; strings are quoted with only '\\', '"' and control characters escaped.</summary>
static class Dumper
{
    public static string Dump(string path)
    {
        var o = new StringBuilder();
        void Line(string text) => o.Append(text).Append('\n');

        using (var file = TagLib.File.Create(path))
        {
            Line($"duration {(int)Math.Round(file.Properties.Duration.TotalSeconds)} rate {file.Properties.AudioSampleRate}");
            if (file.GetTag(TagLib.TagTypes.Id3v2, false) is TagLib.Id3v2.Tag id3)
            {
                Line($"id3v2 version {id3.Version}");
                foreach (var frame in id3) Line("  " + Frame(frame));
            }
            if (file.GetTag(TagLib.TagTypes.Id3v1, false) is TagLib.Id3v1.Tag v1)
                Line($"id3v1 title {Q(v1.Title)} artist {Q(v1.JoinedPerformers)} album {Q(v1.Album)} year {v1.Year} comment {Q(v1.Comment)} track {v1.Track} genre {Q(v1.FirstGenre)}");
            if (file.GetTag(TagLib.TagTypes.Xiph, false) is TagLib.Ogg.XiphComment xiph)
            {
                Line($"xiph vendor {Q(xiph.VendorId)}");
                foreach (var key in xiph)
                {
                    var values = xiph.GetField(key);
                    Line($"  {key} {(IsPictureField(key) ? L(values.Select(BlockPicture)) : L(values.Select(Q)))}");
                }
            }
            if (file.GetTag(TagLib.TagTypes.Apple, false) is TagLib.Mpeg4.AppleTag apple)
            {
                Line("apple");
                foreach (var box in apple) Line("  " + Atom(box));
            }
            if (file.GetTag(TagLib.TagTypes.FlacMetadata, false) is TagLib.Flac.Metadata flac)
                foreach (var picture in flac.Pictures) Line("flacpicture " + FlacPicture((TagLib.Flac.Picture)picture));

            var tag = file.Tag;
            Line("tag");
            Line($"  title {Q(tag.Title)}");
            Line($"  performers {L(tag.Performers.Select(Q))}");
            Line($"  albumartists {L(tag.AlbumArtists.Select(Q))}");
            Line($"  composers {L(tag.Composers.Select(Q))}");
            Line($"  album {Q(tag.Album)}");
            Line($"  genres {L(tag.Genres.Select(Q))}");
            Line($"  year {tag.Year} track {tag.Track}/{tag.TrackCount} disc {tag.Disc}/{tag.DiscCount} bpm {tag.BeatsPerMinute}");
            Line($"  copyright {Q(tag.Copyright)}");
            Line($"  lyrics {Q(tag.Lyrics)}");
            Line($"  isrc {Q(tag.ISRC)} publisher {Q(tag.Publisher)}");
            Line($"  releaseid {Q(tag.MusicBrainzReleaseId)} releasegroupid {Q(tag.MusicBrainzReleaseGroupId)}");
            foreach (var picture in tag.Pictures)
                Line($"  picture {picture.Type} {Q(picture.MimeType)} {Q(picture.Description)} {Data(picture.Data.Data)}");

            Line("extras");
            foreach (var (name, field) in Fields)
                Line($"  {name} {Q(TagWriterExtras.ReadText(file, field))}");
            Line($"  recordingid {Q(TagWriterExtras.ReadRecordingId(file))} compilation {TagWriterExtras.IsCompilation(file)}");
        }

        var facts = TagWriterExtras.ReadFacts(path, true);
        Line($"facts {facts.DurationSeconds} {Q(facts.Extension)} {facts.SampleRate} {Q(facts.Title)} {Q(facts.Artist)} {Q(facts.Album)} {Q(facts.AlbumArtist)} {N(facts.Year)} {N(facts.Track)} {N(facts.Disc)} {L(facts.Isrcs.Select(Q))} {Q(facts.Barcode)} {Q(facts.CatalogNumber)} {Q(facts.Label)} {Q(facts.RecordingId)} {Q(facts.ReleaseId)} {facts.IsCompilation} {facts.TagsAreEvidence}");
        var bare = TagWriterExtras.ReadFacts(path, false);
        Line($"barefacts {bare.DurationSeconds} {Q(bare.Extension)} {bare.SampleRate} {bare.TagsAreEvidence}");
        var (recording, seconds) = TagWriterExtras.ReadIdentity(path);
        Line($"identity {Q(recording)} {seconds}");
        var (album, albumArtist, compilation) = TagWriterExtras.ReadAlbum(path);
        Line($"album {Q(album)} {Q(albumArtist)} {compilation}");
        var kept = KeptIdentityTags.Read(path);
        Line(kept is null ? "kept null" :
            $"kept {Q(kept.Title)} {Q(kept.Album)} {L(kept.AlbumArtist.Select(Q))} {L(kept.AlbumArtists.Select(Q))} {Q(kept.AlbumVersion)} {Q(kept.ReleaseDate)} {Q(kept.AlbumId)} {Q(kept.ReleaseTrackId)} {kept.Track}/{kept.TrackCount} {kept.Disc}/{kept.DiscCount} {kept.Compilation}");
        if (kept is not null) Line($"pid {Q(KeptIdentityTags.PidInputs(kept))}");
        return o.ToString();
    }

    static readonly (string, TagField)[] Fields =
    [
        ("isrc", TagFields.Isrc), ("label", TagFields.Label), ("catalognumber", TagFields.CatalogNumber),
        ("barcode", TagFields.Barcode), ("releasetype", TagFields.ReleaseType), ("releasestatus", TagFields.ReleaseStatus),
        ("releasecountry", TagFields.ReleaseCountry), ("releasetrackid", TagFields.ReleaseTrackId),
        ("albumartistid", TagFields.AlbumArtistId), ("artistid", TagFields.ArtistId), ("acoustid", TagFields.FingerprintId),
        ("trackgain", TagFields.TrackGain), ("trackpeak", TagFields.TrackPeak), ("albumgain", TagFields.AlbumGain),
        ("albumpeak", TagFields.AlbumPeak), ("albumid", TagFields.AlbumId), ("albumartists", TagFields.AlbumArtists),
        ("albumversion", TagFields.AlbumVersion), ("releasedate", TagFields.ReleaseDate),
    ];

    static string Frame(TagLib.Id3v2.Frame frame) => frame switch
    {
        TagLib.Id3v2.UserTextInformationFrame t => $"TXXX enc {(int)t.TextEncoding} desc {Q(t.Description)} {L(t.Text.Select(Q))}",
        TagLib.Id3v2.TextInformationFrame t => $"{Id(t)} enc {(int)t.TextEncoding} {L(t.Text.Select(Q))}",
        TagLib.Id3v2.UniqueFileIdentifierFrame u => $"UFID owner {Q(u.Owner)} id {Q(Encoding.UTF8.GetString(u.Identifier.Data))}",
        TagLib.Id3v2.UnsynchronisedLyricsFrame l => $"USLT enc {(int)l.TextEncoding} lang {Q(l.Language)} desc {Q(l.Description)} text {Q(l.Text)}",
        TagLib.Id3v2.CommentsFrame c => $"COMM enc {(int)c.TextEncoding} lang {Q(c.Language)} desc {Q(c.Description)} text {Q(c.Text)}",
        TagLib.Id3v2.AttachmentFrame a => $"{Id(a)} enc {(int)a.TextEncoding} mime {Q(a.MimeType)} type {a.Type} desc {Q(a.Description)} {Data(a.Data.Data)}",
        _ => $"{Id(frame)} other",
    };

    static string Id(TagLib.Id3v2.Frame frame) => frame.FrameId.ToString(TagLib.StringType.Latin1);

    static string Atom(TagLib.Mpeg4.Box box)
    {
        var name = box.BoxType.ToString(TagLib.StringType.Latin1);
        var parts = new List<string>();
        foreach (var child in box.Children)
        {
            var type = child.BoxType.ToString(TagLib.StringType.Latin1);
            switch (child)
            {
                case TagLib.Mpeg4.AppleAdditionalInfoBox info: parts.Add($"{type} {Q(info.Text)}"); break;
                case TagLib.Mpeg4.AppleDataBox data when data.Flags == 1: parts.Add($"{type}({data.Flags}) {Q(data.Text)}"); break;
                case TagLib.Mpeg4.AppleDataBox data: parts.Add($"{type}({data.Flags}) {Data(data.Data.Data)}"); break;
                default: parts.Add(type); break;
            }
        }
        return $"{Q(name)} {L(parts)}";
    }

    static bool IsPictureField(string key) => key is "METADATA_BLOCK_PICTURE" or "COVERART";

    static string BlockPicture(string base64)
    {
        try { return FlacPicture(new TagLib.Flac.Picture(new TagLib.ByteVector(Convert.FromBase64String(base64)))); }
        catch { return "unreadable " + Q(base64); }
    }

    static string FlacPicture(TagLib.Flac.Picture p) =>
        $"{p.Type} {Q(p.MimeType)} {Q(p.Description)} {p.Width}x{p.Height}x{p.ColorDepth}/{p.IndexedColors} {Data(p.Data.Data)}";

    static string Data(byte[] data) => data.Length <= 16
        ? "hex " + Convert.ToHexString(data).ToLowerInvariant()
        : $"bytes {data.Length} sha256 {Convert.ToHexString(SHA256.HashData(data))[..16].ToLowerInvariant()}";

    static string N(int? value) => value?.ToString(CultureInfo.InvariantCulture) ?? "null";

    static string L(IEnumerable<string> items) => "[" + string.Join(", ", items) + "]";

    public static string Q(string? value)
    {
        if (value is null) return "null";
        var b = new StringBuilder("\"");
        foreach (var c in value)
        {
            if (c == '\\' || c == '"') b.Append('\\').Append(c);
            else if (c < 0x20) b.Append($"\\u{(int)c:x4}");
            else b.Append(c);
        }
        return b.Append('"').ToString();
    }
}
