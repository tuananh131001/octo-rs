// Throwaway reference generator for the Rust list-cover port: lays out and paints covers with
// the real C# code (SixLabors.Fonts + ImageSharp) and writes what it measured and drew, so the
// Rust renderer can be compared with the C# one and not only with the design's Pillow goldens.
// The fallback fonts are whatever the container sees under /usr/share/fonts, so mount the
// host's, read-only, and both builds set the same files. From the repo root, with no .NET SDK
// on the host (it compiles against octo/octo.csproj as octo.Tests, for the internal types; since
// the cutover, restore that tree from csharp-final first, see state-files.md "Regenerating"):
//
//   docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e DOTNET_CLI_HOME=/tmp \
//     -v "$PWD":/repo -v /usr/share/fonts:/usr/share/fonts:ro \
//     -w /repo/docs/rust-migration/fixtures/covers/generator \
//     mcr.microsoft.com/dotnet/sdk:9.0 dotnet run -- /repo/docs/rust-migration/fixtures/covers
//
// It writes reference.json (layouts, advances, font choices) and images/ (lossless WebP of the
// painted covers before JPEG, the goldens' veiled backgrounds, and one served JPEG). Building
// leaves bin/ and obj/ here and under octo/, which are git-ignored.
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Services.CoverArt;
using SixLabors.Fonts;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.Drawing.Processing;
using SixLabors.ImageSharp.Formats.Webp;
using SixLabors.ImageSharp.PixelFormats;

var outDir = args.Length > 0 ? args[0] : "/repo/docs/rust-migration/fixtures/covers";
var goldenPath = args.Length > 1 ? args[1] : "/repo/octo.Tests/CoverGolden/samples.json";
Directory.CreateDirectory(Path.Combine(outDir, "images"));

var book = CoverBook.Default;
var setter = new CoverTypesetter();
var service = new CoverArtService(NullLogger<CoverArtService>.Instance);
var root = new JsonObject();

JsonArray Arr(IEnumerable<JsonNode?> items) => new(items.ToArray());

float Foot(Font font, IReadOnlyList<FontFamily> fallbacks)
{
    var options = new RichTextOptions(font)
    {
        FallbackFontFamilies = fallbacks,
        ColorFontSupport = ColorFontSupport.None,
        KerningMode = KerningMode.Standard,
        VerticalAlignment = VerticalAlignment.Top,
    };
    return TextMeasurer.MeasureBounds("H", options).Bottom;
}

JsonObject WordsJson(CoverWords w)
{
    var (lines, _) = setter.Lines(w.Text, w.Type, w.Width);
    var placed = setter.Place(w).Select((p, i) => (JsonNode?)new JsonObject
    {
        ["text"] = p.Text,
        ["width"] = lines[i].Width,
        ["x"] = p.X,
        ["baseline"] = p.Baseline,
        ["family"] = p.Font.Family.Name,
        ["bold"] = p.Font.IsBold,
        ["ascender"] = p.Font.FontMetrics.HorizontalMetrics.Ascender,
        ["descender"] = p.Font.FontMetrics.HorizontalMetrics.Descender,
        ["unitsPerEm"] = p.Font.FontMetrics.UnitsPerEm,
        ["foot"] = Foot(p.Font, p.Fallbacks),
    });
    return new JsonObject
    {
        ["role"] = w.Role.ToString(),
        ["text"] = w.Text,
        ["size"] = w.Type.SizePx,
        ["weight"] = w.Type.Weight,
        ["lineHeight"] = w.Type.LineHeight,
        ["maxLines"] = w.Type.MaxLines,
        ["left"] = w.Left,
        ["top"] = w.Top,
        ["width"] = w.Width,
        ["align"] = w.Align.ToString(),
        ["ink"] = w.Ink,
        ["lines"] = w.Measured.Lines,
        ["measuredWidth"] = w.Measured.Width,
        ["height"] = w.Measured.Height,
        ["cut"] = w.Measured.Cut,
        ["inked"] = Arr(w.Inked.Select(v => (JsonNode?)v)),
        ["placed"] = Arr(placed),
    };
}

var layouts = new JsonArray();
var images = new JsonArray();

void Save(Image<Rgb24> image, string file)
{
    image.SaveAsWebp(Path.Combine(outDir, "images", file), new WebpEncoder { FileFormat = WebpFileFormatType.Lossless, Quality = 100, Method = WebpEncodingMethod.Level6 });
}

void Layout(string group, CoverSpec spec, CoverArt art, string? image = null, bool veilImage = false)
{
    layouts.Add(new JsonObject
    {
        ["group"] = group,
        ["id"] = spec.Id,
        ["name"] = spec.Name,
        ["line"] = spec.Line,
        ["footer"] = spec.Footer,
        ["music"] = spec.Music is null ? null : new JsonObject { ["hue"] = spec.Music.Hue, ["chroma"] = spec.Music.Chroma, ["lightness"] = spec.Music.Lightness },
        ["side"] = art.Side,
        ["background"] = art.Background,
        ["orientation"] = art.Orientation,
        ["words"] = Arr(art.Words.Select(w => (JsonNode?)WordsJson(w))),
        ["image"] = image,
        ["veilImage"] = veilImage ? "veil-" + image : null,
    });
    if (image is not null)
    {
        using var painted = CoverPainter.Paint(book, art, setter);
        Save(painted, image);
        if (veilImage)
        {
            using var veiled = CoverPainter.Paint(book, art, setter, drawWords: false);
            Save(veiled, "veil-" + image);
        }
    }
}

// A. The design's goldens, set as the C# golden test sets them.
var golden = JsonDocument.Parse(File.ReadAllText(goldenPath)).RootElement.GetProperty("covers");
foreach (var g in golden.EnumerateArray())
{
    var side = g.GetProperty("side").GetInt32();
    string? Text(string key) => g.GetProperty(key).ValueKind == JsonValueKind.Null ? null : g.GetProperty(key).GetString();
    var spec = new CoverSpec(g.GetProperty("name").GetString()!, g.GetProperty("name").GetString()!, Text("line"), Text("footer"), null);
    var file = g.GetProperty("background").GetString();
    var index = book.Backgrounds.Select((b, i) => (b, i)).Single(pair => pair.b.File == file).i;
    var art = new CoverArt(side, index, 0, CoverLayout.Words(spec, side, setter, book));
    Layout("golden", spec, art, Path.ChangeExtension(g.GetProperty("file").GetString()!, ".webp"), veilImage: true);
}

// B. The names ListCoverTests fits inside the margins.
string[] names =
[
    "Rock", "Red Hot Chili Peppers", "The Most Unreasonably Long Playlist Name Anyone Ever Typed Into A Music Server",
    "Supercalifragilisticexpialidociousness", "宇多田ヒカル", "블랙핑크 BLACKPINK", "فيروز", "שירים ישנים",
    "Late Night 🌙 Chill", "Ünïcödé Café",
];
foreach (var name in names)
foreach (var side in new[] { 600, 1200 })
{
    var spec = CoverArtService.Spec(name + " Radio", ListKinds.Radio, 120, null);
    Layout("names", spec, service.Compose(spec, side), side == 600 ? $"names-{Array.IndexOf(names, name)}-600.webp" : null);
}

// C. Long, cut and right-to-left names.
foreach (var (name, kind) in new[]
         {
             ("Red Hot Chili Peppers And Friends Radio", ListKinds.Radio),
             (string.Join(" ", Enumerable.Repeat("Unreasonably", 12)), ListKinds.Mix),
             ("فيروز Radio", ListKinds.Radio),
         })
{
    var spec = CoverArtService.Spec(name, kind, null, null);
    Layout("long", spec, service.Compose(spec, 600));
}

// D. Names in scripts Inter does not have.
var unicode = new[] { "宇多田ヒカル", "블랙핑크", "فيروز", "שירים", "🌙🎧" };
for (var i = 0; i < unicode.Length; i++)
{
    var spec = CoverArtService.Spec(unicode[i], ListKinds.Mix, null, null);
    Layout("unicode", spec, service.Compose(spec, 600), $"unicode-{i}-600.webp");
}

// E. The contrast test's cover.
foreach (var side in new[] { 600, 1200 })
{
    var spec = CoverArtService.Spec("Everything I Have Ever Loved Radio", ListKinds.Radio, 1234, null);
    Layout("contrast", spec, service.Compose(spec, side), side == 600 ? "contrast-600.webp" : null);
}

// F. The contact sheet's lists, with their music.
(string Name, int Hue, double Chroma, double Lightness)[] sheet =
[
    ("Daft Punk Radio", 261, 0.043, 0.722), ("Billie Eilish Radio", 63, 0.061, 0.575),
    ("Tame Impala Radio", 318, 0.041, 0.453), ("Radiohead Radio", 47, 0.159, 0.657),
    ("Kendrick Lamar Radio", 4, 0.068, 0.41), ("Your Mix", 241, 0.039, 0.732),
    ("Discovery Mix", 30, 0.225, 0.581), ("Bad Bunny Radio", 30, 0.225, 0.581),
    ("Jazz & Blues Mix", 225, 0.14, 0.62), ("Metal Mix", 40, 0.163, 0.601),
    ("1970s Mix", 75, 0.14, 0.62), ("Rock Mix", 26, 0.14, 0.62),
    ("Hip-Hop Mix", 61, 0.14, 0.62), ("1990s Mix", 134, 0.14, 0.62),
    ("2020s Mix", 168, 0.14, 0.62), ("Electronic Radio", 250, 0.14, 0.62),
    ("Polka Mix", -1, 0, 0), ("Red Hot Chili Peppers Radio", -1, 0, 0),
    ("The Most Unreasonably Long Playlist Name Anyone Ever Typed Into A Music Server Radio", -1, 0, 0),
    ("宇多田ヒカル Radio", -1, 0, 0), ("블랙핑크 BLACKPINK Radio", 5, 0.041, 0.336),
    ("فيروز Radio", 69, 0.065, 0.682), ("Late Night 🌙 Chill Mix", 206, 0.14, 0.62),
    ("Ünïcödé Café Mix", -1, 0, 0),
];
for (var i = 0; i < sheet.Length; i++)
{
    var list = sheet[i];
    var kind = list.Name.EndsWith(" Radio") ? ListKinds.Radio : ListKinds.Mix;
    var music = list.Hue < 0 ? null : CoverMusic.Of(list.Hue, list.Chroma, list.Lightness);
    var spec = CoverArtService.Spec(list.Name, kind, 50, music);
    Layout("sheet", spec, service.Compose(spec, 600), i % 4 == 0 ? $"sheet-{i}-600.webp" : null);
}

// G. Names that already say what they are.
foreach (var (name, kind) in new[]
         {
             ("Your Mix", ListKinds.Radio), ("Discovery Mix", ListKinds.Radio), ("Late Night Radio Station", ListKinds.Radio),
             ("Road Trip Playlist", ListKinds.Mix), ("Summer mixes", ListKinds.Mix), ("Pirate Radios", ListKinds.Radio),
         })
{
    var spec = CoverArtService.Spec(name, kind, 50, null);
    Layout("kind", spec, service.Compose(spec, 600));
}

root["layouts"] = layouts;

// Widths of some text on one line, as the fitting asks for them.
var advances = new JsonArray();
foreach (var weight in new[] { 600, 300, 500 })
foreach (var size in new[] { 36f, 79f, 96f, 138f })
foreach (var text in new[] { "Gym", "Playlist", "By Noah", "AV", "Wa", "office", "Everything I", "1,204 songs", "宇多田ヒカル", "فيروز", "🌙", "Ünïcödé Café" })
    advances.Add(new JsonObject { ["text"] = text, ["weight"] = weight, ["size"] = size, ["width"] = CoverTypesetter.Advance(text, new CoverType(size, weight, 0, 1, 1)) });
root["advances"] = advances;

// Which font each text is set in.
var choices = new JsonArray();
foreach (var text in new[] { "Rock", "宇多田ヒカル", "블랙핑크", "فيروز", "שירים", "🌙🎧", "Late Night 🌙 Chill", "블랙핑크 BLACKPINK", "Ünïcödé Café" })
foreach (var weight in new[] { 600, 300, 500 })
{
    var (font, fallbacks) = CoverFonts.For(text, weight, 96);
    choices.Add(new JsonObject
    {
        ["text"] = text, ["weight"] = weight, ["family"] = font.Family.Name, ["bold"] = font.IsBold,
        ["fallbacks"] = Arr(fallbacks.Select(f => (JsonNode?)f.Name)),
    });
}
root["choices"] = choices;
root["fallbacks"] = Arr(CoverFonts.Fallbacks.Select(f => (JsonNode?)f.Name));

// One cover as the server serves it, JPEG and all.
var served = service.Render(CoverArtService.Spec("Rock Radio", ListKinds.Radio, null, service.FallbackMusic("Rock Radio", "Rock Radio")), 600);
File.WriteAllBytes(Path.Combine(outDir, "images", "rock-radio-600.jpg"), served);

File.WriteAllText(Path.Combine(outDir, "reference.json"), root.ToJsonString(new JsonSerializerOptions { WriteIndented = true, Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping }));
Console.WriteLine($"wrote {layouts.Count} layouts to {outDir}");
