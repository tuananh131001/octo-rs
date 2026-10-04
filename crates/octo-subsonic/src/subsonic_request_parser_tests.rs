//! Port of `SubsonicRequestParserTests`, plus the ASP.NET parsing rules the port relies on,
//! each checked against .NET 9 (`DefaultHttpContext` + the C# parser) on 2026-10-04.

use super::*;

fn query(q: &str) -> RequestParts<'_> {
    RequestParts {
        query: Some(q),
        ..Default::default()
    }
}

fn with_body<'a>(q: Option<&'a str>, content_type: &'a str, body: &'a str) -> RequestParts<'a> {
    RequestParts {
        query: q,
        content_type: Some(content_type),
        content_length: Some(body.len() as u64),
        body: body.as_bytes(),
    }
}

fn pairs(parameters: &Parameters) -> Vec<(&str, &str)> {
    parameters.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[test]
fn extract_all_parameters_query_parameters_extracts_correctly() {
    let result = extract_all_parameters(&query("u=admin&p=password&v=1.16.0&c=testclient&f=json"));

    assert_eq!(result.len(), 5);
    assert_eq!(result["u"], "admin");
    assert_eq!(result["p"], "password");
    assert_eq!(result["v"], "1.16.0");
    assert_eq!(result["c"], "testclient");
    assert_eq!(result["f"], "json");
}

#[test]
fn extract_all_parameters_form_encoded_body_extracts_correctly() {
    let result = extract_all_parameters(&with_body(
        None,
        "application/x-www-form-urlencoded",
        "u=admin&p=password&query=test+artist&artistCount=10",
    ));

    assert_eq!(result.len(), 4);
    assert_eq!(result["u"], "admin");
    assert_eq!(result["p"], "password");
    assert_eq!(result["query"], "test artist");
    assert_eq!(result["artistCount"], "10");
}

#[test]
fn extract_all_parameters_json_body_extracts_correctly() {
    let result = extract_all_parameters(&with_body(
        None,
        "application/json",
        r#"{"u":"admin","p":"password","query":"test artist","artistCount":10}"#,
    ));

    assert_eq!(result.len(), 4);
    assert_eq!(result["u"], "admin");
    assert_eq!(result["p"], "password");
    assert_eq!(result["query"], "test artist");
    assert_eq!(result["artistCount"], "10");
}

#[test]
fn extract_all_parameters_query_and_form_body_merges_correctly() {
    let result = extract_all_parameters(&with_body(
        Some("u=admin&p=password&f=json"),
        "application/x-www-form-urlencoded",
        "query=test&artistCount=5",
    ));

    assert_eq!(result.len(), 5);
    assert_eq!(result["u"], "admin");
    assert_eq!(result["p"], "password");
    assert_eq!(result["f"], "json");
    assert_eq!(result["query"], "test");
    assert_eq!(result["artistCount"], "5");
}

#[test]
fn extract_all_parameters_empty_request_returns_empty_dictionary() {
    assert!(extract_all_parameters(&RequestParts::default()).is_empty());
}

#[test]
fn extract_all_parameters_special_characters_encodes_correctly() {
    let result = extract_all_parameters(&query("query=rock+%26+roll&artist=AC%2FDC"));

    assert_eq!(result.len(), 2);
    assert_eq!(result["query"], "rock & roll");
    assert_eq!(result["artist"], "AC/DC");
}

#[test]
fn extract_all_parameters_invalid_json_ignores_body() {
    let result = extract_all_parameters(&with_body(Some("u=admin"), "application/json", "{invalid json}"));

    assert_eq!(pairs(&result), [("u", "admin")]);
}

#[test]
fn extract_all_parameters_null_json_values_handles_gracefully() {
    let result = extract_all_parameters(&with_body(
        None,
        "application/json",
        r#"{"u":"admin","p":null,"query":"test"}"#,
    ));

    assert_eq!(result.len(), 3);
    assert_eq!(result["u"], "admin");
    assert_eq!(result["p"], "");
    assert_eq!(result["query"], "test");
}

#[test]
fn extract_all_parameters_duplicate_keys_body_overrides_query() {
    let result = extract_all_parameters(&with_body(
        Some("format=xml&query=old"),
        "application/json",
        r#"{"query":"new","artist":"Beatles"}"#,
    ));

    assert_eq!(result.len(), 3);
    assert_eq!(result["format"], "xml");
    assert_eq!(result["query"], "new"); // Body overrides query
    assert_eq!(result["artist"], "Beatles");
}

// What follows pins ASP.NET's own parsing, each case run through .NET 9 first.

#[test]
fn query_keys_merge_ignoring_case_under_the_first_spelling() {
    let result = extract_all_parameters(&query("u=a&U=b&f=json&F=xml"));
    assert_eq!(pairs(&result), [("u", "a,b"), ("f", "json,xml")]);
}

#[test]
fn query_segments_follow_query_feature() {
    let cases: [(&str, &[(&str, &str)]); 5] = [
        (
            "a=1&&b=2&=3&c&d=",
            &[("a", "1"), ("b", "2"), ("", "3"), ("c", ""), ("d", "")],
        ),
        (
            "q=a+b%2Bc%20d&bad=%ZZ&ff=%FF&e9=%C3%A9&lone=%&k%41=1",
            &[
                ("q", "a b+c d"),
                ("bad", "%ZZ"),
                ("ff", "%FF"),
                ("e9", "é"),
                ("lone", "%"),
                ("kA", "1"),
            ],
        ),
        // StringValues.ToString leaves the empty values out.
        ("id=&id=B&id=", &[("id", "B")]),
        ("a=1;b=2", &[("a", "1;b=2")]),
        ("x=%C3%A9%FF%41&y=%ff", &[("x", "é%FFA"), ("y", "%ff")]),
    ];
    for (q, expected) in cases {
        assert_eq!(pairs(&extract_all_parameters(&query(q))), expected, "{q}");
    }
}

#[test]
fn the_query_collection_keeps_every_value() {
    let collection = parse_query("id=&id=B&ID=");
    assert_eq!(
        collection.get("Id"),
        Some(&["".to_string(), "B".into(), "".into()][..])
    );
    assert_eq!(join_values(collection.get("id").unwrap_or_default()), "B");
}

#[test]
fn form_fields_merge_ignoring_case_but_stay_apart_from_query_keys() {
    let result = extract_all_parameters(&with_body(
        Some("u=q"),
        "application/x-www-form-urlencoded",
        "U=f1&u=f2&p=x+y%21&k&=e&&z=%FF",
    ));
    assert_eq!(
        pairs(&result),
        [
            ("u", "q"),
            ("U", "f1,f2"),
            ("p", "x y!"),
            ("k", ""),
            ("", "e"),
            ("z", "%FF")
        ]
    );
}

#[test]
fn the_form_content_type_is_matched_exactly_ignoring_case() {
    for (content_type, is_form) in [
        ("application/x-www-form-urlencoded; charset=utf-8", true),
        ("Application/X-WWW-Form-UrlEncoded", true),
        ("application/x-www-form-urlencoded;", true),
        ("application/x-www-form-urlencodedx", false),
    ] {
        let result = extract_all_parameters(&with_body(None, content_type, "a=1"));
        let expected: &[(&str, &str)] = if is_form { &[("a", "1")] } else { &[] };
        assert_eq!(pairs(&result), expected, "{content_type}");
    }
}

#[test]
fn multipart_fields_are_read_and_files_left_out() {
    let body = "--XX\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nv1\r\n--XX\r\n\
                Content-Disposition: form-data; name=\"A\"\r\n\r\nv2\r\n--XX\r\n\
                Content-Disposition: form-data; name=\"f\"; filename=\"x.txt\"\r\nContent-Type: text/plain\r\n\r\nfile\r\n--XX--\r\n";
    let result = extract_all_parameters(&with_body(None, "multipart/form-data; boundary=XX", body));
    assert_eq!(pairs(&result), [("a", "v1,v2")]);

    let quoted = "--XX\r\nContent-Disposition: form-data; name=a\r\n\r\nv1\r\n--XX--";
    let result = extract_all_parameters(&with_body(None, "multipart/form-data; boundary=\"XX\"", quoted));
    assert_eq!(pairs(&result), [("a", "v1")]);
}

#[test]
fn a_multipart_form_that_cannot_be_read_falls_back_as_asp_net_did() {
    // No boundary: the reader threw before reading, so the body was parsed as a query string.
    let result = extract_all_parameters(&with_body(None, "multipart/form-data", "a=1&b=2"));
    assert_eq!(pairs(&result), [("a", "1"), ("b", "2")]);
    // A boundary the body never uses: the reader consumed the body, so nothing is left.
    let result = extract_all_parameters(&with_body(
        None,
        "multipart/form-data; boundary=XX",
        "garbage a=1",
    ));
    assert!(result.is_empty());
}

#[test]
fn json_values_are_json_element_to_string() {
    let result = extract_all_parameters(&with_body(
        None,
        "application/json",
        r#"{"s":"xA","n":1.50,"e":1e3,"t":true,"f":false,"z":null,"o":{ "a" : [1, 2] },"arr":[ ]}"#,
    ));
    assert_eq!(
        pairs(&result),
        [
            ("s", "xA"),
            ("n", "1.50"),
            ("e", "1e3"),
            ("t", "True"),
            ("f", "False"),
            ("z", ""),
            ("o", r#"{ "a" : [1, 2] }"#),
            ("arr", "[ ]"),
        ]
    );
    let escaped = extract_all_parameters(&with_body(None, "application/json", r#"{"a":"🎵 \"q\" \/"}"#));
    assert_eq!(pairs(&escaped), [("a", "🎵 \"q\" /")]);
}

#[test]
fn json_bodies_are_read_as_system_text_json_read_them() {
    let deep = |levels: usize| format!("{{\"a\":{}{}}}", "[".repeat(levels), "]".repeat(levels));
    let deepest_kept = format!("{}{}", "[".repeat(63), "]".repeat(63));
    let cases: Vec<(String, &str, String, Vec<(&str, &str)>)> = vec![
        // A repeated member: the last value, in the first one's place.
        (
            "".into(),
            "application/json",
            r#"{"a":"1","b":2,"a":"3"}"#.into(),
            vec![("a", "3"), ("b", "2")],
        ),
        ("x=1".into(), "application/json", "[1,2]".into(), vec![("x", "1")]),
        // The content type check is an ordinal Contains.
        ("".into(), "Application/JSON", r#"{"a":1}"#.into(), vec![]),
        (
            "".into(),
            "text/plain; application/json",
            r#"{"a":1}"#.into(),
            vec![("a", "1")],
        ),
        (
            "".into(),
            "application/json",
            "\u{feff}{\"a\":1}".into(),
            vec![("a", "1")],
        ),
        ("".into(), "application/json", r#"{"a":1} x"#.into(), vec![]),
        (
            "".into(),
            "application/json",
            "  {\"a\":1}  \n".into(),
            vec![("a", "1")],
        ),
        ("q=1".into(), "application/json", "null".into(), vec![("q", "1")]),
        ("q=1".into(), "application/json", "".into(), vec![("q", "1")]),
        // Sixty-four levels with the object itself, and no more.
        (
            "".into(),
            "application/json",
            deep(63),
            vec![("a", deepest_kept.as_str())],
        ),
        ("".into(), "application/json", deep(64), vec![]),
        ("".into(), "application/json", deep(70), vec![]),
        // The body overrides a query value in the query value's place.
        (
            "b=q&a=q".into(),
            "application/json",
            r#"{"a":"j","c":"j"}"#.into(),
            vec![("b", "q"), ("a", "j"), ("c", "j")],
        ),
        // The dictionary is ordinal, so a key in another case is another key.
        (
            "A=q".into(),
            "application/json",
            r#"{"a":"j"}"#.into(),
            vec![("A", "q"), ("a", "j")],
        ),
    ];
    for (q, content_type, body, expected) in cases {
        let result = extract_all_parameters(&with_body(Some(&q), content_type, &body));
        assert_eq!(pairs(&result), expected, "{content_type} {body}");
    }
}

#[test]
fn a_content_type_with_no_body_reads_only_the_query() {
    let result = extract_all_parameters(&RequestParts {
        query: Some("a=1"),
        content_type: Some("text/plain"),
        content_length: None,
        body: b"",
    });
    assert_eq!(pairs(&result), [("a", "1")]);
}

#[test]
fn extract_parameter_values_keeps_repeats_apart_and_skips_blanks() {
    let request = with_body(
        Some("id=A&ID=%20&id=B"),
        "application/x-www-form-urlencoded",
        "Id=C&id=&time=1",
    );
    assert_eq!(extract_parameter_values(&request, "id"), ["A", "B", "C"]);
    assert_eq!(extract_parameter_values(&request, "time"), ["1"]);
    assert!(extract_parameter_values(&request, "submission").is_empty());
}

#[test]
fn escape_data_string_matches_uri_escape_data_string() {
    assert_eq!(
        escape_data_string("a b+c/é~-_.!*'()"),
        "a%20b%2Bc%2F%C3%A9~-_.%21%2A%27%28%29"
    );
}
