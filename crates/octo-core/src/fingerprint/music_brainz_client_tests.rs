//! Ports of `octo.Tests/MusicBrainzStudioAlbumTests.cs`, the `MusicBrainzQueryTests` class of
//! `octo.Tests/MusicBrainzReleaseDetailsTests.cs`, and `ParseIsrcs_ReadsARecordingLookup` of
//! `octo.Tests/IsrcVerificationTests.cs`.

use super::*;

fn studio(json: &str, title: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(json).expect("test JSON parses");
    pick_studio_album(&doc, title).expect("the answer reads")
}

// ---- MusicBrainzStudioAlbumTests ---------------------------------------------------------

#[test]
fn picks_earliest_studio_album_over_singles_compilations_and_other_recordings() {
    let search = r#"
    {"recordings":[
      {"title":"Zukunft Pink","releases":[
        {"date":"2022-10-20","release-group":{"id":"single","primary-type":"Single"}},
        {"date":"2023-02-10","release-group":{"id":"bravo","primary-type":"Album","secondary-types":["Compilation"]}}]},
      {"title":"Zukunft Pink","releases":[
        {"date":"2024-06-21","release-group":{"id":"love-songs","primary-type":"Album"}},
        {"date":"","release-group":{"id":"empty-date","primary-type":"Album","secondary-types":[]}},
        {"release-group":{"id":"undated","primary-type":"Album","secondary-types":[]}}]},
      {"title":"Zukunft Pink (Remix)","releases":[
        {"date":"1999-01-01","release-group":{"id":"remix-album","primary-type":"Album"}}]}
    ]}
    "#;

    assert_eq!(studio(search, "Zukunft Pink").as_deref(), Some("love-songs"));
}

#[test]
fn matches_title_ignoring_punctuation_and_case() {
    let search = r#"
    {"recordings":[{"title":"They Don't Care About Us","releases":[
      {"date":"1995-06-20","release-group":{"id":"history","primary-type":"Album"}}]}]}
    "#;

    assert_eq!(
        studio(search, "They Dont Care About Us").as_deref(),
        Some("history")
    );
}

#[test]
fn falls_back_to_soundtrack_only_without_studio_album() {
    let soundtrack_only = r#"
    {"recordings":[{"title":"Blub","releases":[
      {"date":"2021-09-17","release-group":{"id":"toem","primary-type":"Album","secondary-types":["Soundtrack"]}},
      {"date":"2020-01-01","release-group":{"id":"best-of","primary-type":"Album","secondary-types":["Compilation"]}}]}]}
    "#;
    let both = r#"
    {"recordings":[{"title":"Song","releases":[
      {"date":"1990-01-01","release-group":{"id":"film","primary-type":"Album","secondary-types":["Soundtrack"]}},
      {"date":"1995-01-01","release-group":{"id":"studio","primary-type":"Album"}}]}]}
    "#;

    assert_eq!(studio(soundtrack_only, "Blub").as_deref(), Some("toem"));
    assert_eq!(studio(both, "Song").as_deref(), Some("studio"));
}

#[test]
fn returns_null_when_only_singles_and_compilations() {
    let search = r#"
    {"recordings":[{"title":"Zukunft Pink","releases":[
      {"date":"2022-10-20","release-group":{"id":"single","primary-type":"Single"}},
      {"date":"2023-02-10","release-group":{"id":"bravo","primary-type":"Album","secondary-types":["Compilation"]}}]}]}
    "#;

    assert_eq!(studio(search, "Zukunft Pink"), None);
}

// ---- MusicBrainzQueryTests ---------------------------------------------------------------
// Titles and names with quotes, colons, slashes or brackets must reach the music database as
// the words they are, not as operators, or the failure reads as "no candidate".

fn query(url: &str) -> String {
    let start = url.find("query=").expect("has a query") + "query=".len();
    let end = url.find("&fmt=json").expect("has fmt");
    dotnet::unescape_data_string(&url[start..end])
}

#[test]
fn build_recording_search_url_escapes_every_operator() {
    let cases: &[(&str, &str, i32, &str)] = &[
        (
            "AC/DC",
            "Thunderstruck",
            292,
            "recording:\"Thunderstruck\" AND artist:\"AC\\/DC\" AND dur:[282000 TO 302000]",
        ),
        (
            "Bizarrap",
            "Bzrp Music Sessions, Vol. 56",
            0,
            "recording:\"Bzrp Music Sessions, Vol. 56\" AND artist:\"Bizarrap\"",
        ),
        (
            "Adele",
            "Hello?",
            295,
            "recording:\"Hello\\?\" AND artist:\"Adele\" AND dur:[285000 TO 305000]",
        ),
        (
            "Shawn Mendes",
            "Señorita: Remix",
            0,
            "recording:\"Señorita\\: Remix\" AND artist:\"Shawn Mendes\"",
        ),
        (
            "\"Weird Al\" Yankovic",
            "Amish Paradise",
            0,
            "recording:\"Amish Paradise\" AND artist:\"\\\"Weird Al\\\" Yankovic\"",
        ),
    ];
    for &(artist, title, seconds, expected) in cases {
        assert_eq!(
            query(&build_recording_search_url(artist, title, seconds)),
            expected,
            "{artist} - {title}"
        );
    }
}

#[test]
fn build_recording_search_url_exact_encoded_url() {
    assert_eq!(
        build_recording_search_url("AC/DC", "Thunderstruck", 292),
        "recording/?query=recording%3A%22Thunderstruck%22%20AND%20artist%3A%22AC%5C%2FDC%22%20AND%20dur%3A%5B282000%20TO%20302000%5D&fmt=json&limit=25"
    );
}

#[test]
fn build_recording_search_url_short_length_never_goes_negative() {
    assert!(query(&build_recording_search_url("A", "B", 5)).ends_with("AND dur:[0 TO 15000]"));
}

#[test]
fn escape_query_cases() {
    let cases = [
        ("plain words", "plain words"),
        (
            "a+b-c&&d||e!f(g)h{i}j[k]l^m\"n~o*p?q:r\\s/t",
            "a\\+b\\-c\\&\\&d\\|\\|e\\!f\\(g\\)h\\{i\\}j\\[k\\]l\\^m\\\"n\\~o\\*p\\?q\\:r\\\\s\\/t",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(escape_query(input), expected, "{input}");
    }
}

// ---- IsrcVerificationTests ---------------------------------------------------------------

#[test]
fn parse_isrcs_reads_a_recording_lookup() {
    let doc: Value = serde_json::from_str(
        r#"{"id":"mbid","title":"紅蓮華","isrcs":["JPU901901234","jpu901901234","bogus"]}"#,
    )
    .expect("parses");
    assert_eq!(parse_isrcs(&doc).expect("reads"), ["JPU901901234"]);

    let none: Value = serde_json::from_str(r#"{"id":"mbid","title":"紅蓮華"}"#).expect("parses");
    assert!(parse_isrcs(&none).expect("reads").is_empty());
}

// ---- Rust-only checks of the other two queries and Pick -----------------------------------

#[test]
fn find_recording_and_studio_album_queries_are_shaped_as_the_csharp_wrote_them() {
    let url = find_recording_url("Massive Attack", "Tear\"drop", 330).expect("asks");
    assert_eq!(
        query(&url),
        "recording:\"Tear\\\"drop\" AND artist:\"Massive Attack\""
    );
    assert!(find_recording_url("A", "B", 0).is_none());

    let (url, plain) =
        studio_album_search("Michael Jackson", "They Don't Care About Us (feat. X)").expect("asks");
    assert_eq!(
        plain,
        SongIdentity::strip_features("They Don't Care About Us (feat. X)")
    );
    assert!(query(&url).starts_with("recording:(They Don t Care About Us"));
    assert!(url.ends_with("&fmt=json&limit=50"));
    assert!(studio_album_search("A", "!!!").is_none());
}

#[test]
fn pick_answers_only_when_exactly_one_recording_fits() {
    let doc: Value = serde_json::from_str(
        r#"{"recordings":[
          {"id":"a","score":100,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"A","score":100,"title":"Teardrop","length":330500,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"live","score":100,"title":"Teardrop","disambiguation":"live","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"low","score":50,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"video","score":100,"video":true,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"long","score":100,"title":"Teardrop","length":400000,"artist-credit":[{"name":"Massive Attack"}]}
        ]}"#,
    )
    .expect("parses");
    // "a" and "A" are one id to an OrdinalIgnoreCase set.
    assert_eq!(
        pick(&doc, "Massive Attack", "Teardrop", 330)
            .expect("reads")
            .as_deref(),
        Some("a")
    );

    let two: Value = serde_json::from_str(
        r#"{"recordings":[
          {"id":"a","title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"b","title":"Teardrop","length":330027,"artist-credit":[{"name":"Massive Attack"}]}
        ]}"#,
    )
    .expect("parses");
    assert_eq!(
        pick(&two, "Massive Attack", "Teardrop", 330).expect("reads"),
        None
    );
}
