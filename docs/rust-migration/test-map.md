# Test map: xUnit → cargo test

Each of the 125 C# test files in `octo.Tests/` (3,395 tests at `csharp-final`; one of them,
`LastFmRadioControllerTests.InternetRadioList_AnswersInsideTheStarterBoundAndPublishesOnTheNextRefresh`,
fails intermittently on a timeout in the baseline run) maps to a Rust location here.

| C# file | C# tests | Rust location | Rust tests | Dropped / notes |
|---|---:|---|---:|---|
