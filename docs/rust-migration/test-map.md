# Test map: xUnit → cargo test

Each of the 125 C# test files in `octo.Tests/` (3,395 tests at `csharp-final`; one of them,
`LastFmRadioControllerTests.InternetRadioList_AnswersInsideTheStarterBoundAndPublishesOnTheNextRefresh`,
fails intermittently on a timeout in the baseline run) maps to a Rust location here.

| C# file | C# tests | Rust location | Rust tests | Dropped / notes |
|---|---:|---|---:|---|
| `ListCoverTests.cs` (`CoverColourTests`, `ListCoverTests`, `CoverTimingTests`) | 128 | `crates/octo-media/src/cover/list_cover_tests.rs` | 37 | Each `[Theory]` is one test over its case table (the 48-background contrast theory included). `UnicodeNames_DrawTheirLetters` skips a case, with a printed reason, when no installed font draws its characters (the emoji case without Symbola), as tests needing ffmpeg skip. The seed fixture's identity adds a digest of the picture, since the `image` crate's PNGs of two flat pictures can share the length and fifth-last byte C# keyed on. The timing test holds the heavy-test lock for writing instead of a non-parallel collection. |
| `ListCoverTests.cs` (`CoverGoldenTests`) | 18 | `crates/octo-media/src/cover/golden_tests.rs` | 3 | The three theories over the six goldens of `CoverGolden/samples.json`, same tolerances (sizes, lines, x, top, height to 0.01 px; right edges within max(2, 2%); veil samples within 2, server widths within 4). Added: 6 tests against the C# renderer's own output in `docs/rust-migration/fixtures/covers/` (layouts, advances, font choices, veils pixel for pixel, finished covers at SSIM ≥ 0.98 with diff images under `target/cover-golden-diffs/`, the served JPEG). Cases set in system fonts are skipped when the machine's fallback fonts differ from the reference's. |
