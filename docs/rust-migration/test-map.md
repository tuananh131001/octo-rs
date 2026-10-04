# Test map: xUnit → cargo test

Each of the 125 C# test files in `octo.Tests/` (3,395 tests at `csharp-final`; one of them,
`LastFmRadioControllerTests.InternetRadioList_AnswersInsideTheStarterBoundAndPublishesOnTheNextRefresh`,
fails intermittently on a timeout in the baseline run) maps to a Rust location here.

| C# file | C# tests | Rust location | Rust tests | Dropped / notes |
|---|---:|---|---:|---|
| `LoudnessMeterTests.cs` | 11 | `crates/octo-media/src/audio/loudness_meter.rs` (`tests`) | 15 | All 11 ported. `Formats_UseADotWhateverTheCulture` pins the invariant texts (Rust formatting has no culture). 4 Rust-only: zero/positive gain signs, a non-numeric level, and real ffmpeg on a generated tone (measured, and cancelled), which skip without ffmpeg. |
| `AudioFingerprinterTests.cs` | 9 (4 methods) | `crates/octo-media/src/audio/audio_fingerprinter.rs` (`tests`) | 6 | All 4 methods ported; the 6-row theory is one table test. 2 Rust-only: the missing-fpcalc latch (skips if fpcalc is installed) and real fpcalc on generated noise (skips if fpcalc is missing). |
| `SpectrumAnalyzerTests.cs` | 33 (19 methods) | `crates/octo-media/src/audio/spectrum_analyzer_tests.rs` | 15 | `SpectrumAnalyzerTests` (26 cases, 13 methods): all ported, theories as table tests; the 6 `[FfmpegFact]` tests generate the same ffmpeg audio per test instead of a class fixture, and skip without ffmpeg. `TranscodeDecisionTests` (7 cases, 6 methods, same file) tests `SoulseekDownloadService.WeighTranscode` and `LibraryActionExecutor.NotReallyLossless`, so it belongs to those porters; the `SpectrumReport.Describe`/`IsLikelyLossy` part is ported here as 2 tests. |
| (none: Rust-only) | 0 | `crates/octo-media/src/audio/net_format.rs` (`tests`) | 3 | .NET `Math.Round`, custom numeric formats and `Path.GetExtension` as the audio tools use them. |
