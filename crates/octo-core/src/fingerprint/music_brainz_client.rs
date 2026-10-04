//! The pure half of `Services/Fingerprint/MusicBrainzClient.cs`: the query builders and the
//! readers of its answers. The HTTP half (the one-a-second gate and the cache) is
//! `octo::services::fingerprint::music_brainz_client`.

use serde_json::Value;

use crate::common::SongIdentity;
use crate::common::dotnet;
use crate::fingerprint::track_match_comparer::TrackMatchComparer;
use crate::json::element::{
    ElementResult, enumerate_array, get_double, get_int32, get_property, get_string, try_get_property,
};

/// The search for a recording by name, with a length window of ten seconds either way when
/// the length is known. Every name goes through [`escape_query`], since a quote, a colon or a
/// slash in a title would otherwise change what the query means and the failure would read as
/// "no candidate".
pub fn build_recording_search_url(artist: &str, title: &str, duration_seconds: i32) -> String {
    let mut parts = Vec::new();
    if !dotnet::is_blank(title) {
        parts.push(format!("recording:\"{}\"", escape_query(title.trim())));
    }
    if !dotnet::is_blank(artist) {
        parts.push(format!("artist:\"{}\"", escape_query(artist.trim())));
    }
    if duration_seconds > 0 {
        let low = (duration_seconds - 10).max(0) * 1000;
        let high = (duration_seconds + 10) * 1000;
        parts.push(format!("dur:[{low} TO {high}]"));
    }
    format!(
        "recording/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&parts.join(" AND "))
    )
}

/// Every character the query language reads as an operator, made literal.
pub fn escape_query(value: &str) -> String {
    const SPECIAL: &str = "+-&|!(){}[]^\"~*?:\\/";
    let mut sb = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        if SPECIAL.contains(ch) {
            sb.push('\\');
        }
        sb.push(ch);
    }
    sb
}

/// The quoting `FindRecordingAsync` and `FindStudioAlbumAsync` use: backslashes and quotes only.
pub fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The relative URL `FindRecordingAsync` asks, or None when the song cannot be asked for.
pub fn find_recording_url(artist: &str, title: &str, duration_seconds: i32) -> Option<String> {
    if dotnet::is_blank(artist) || dotnet::is_blank(title) || duration_seconds <= 0 {
        return None;
    }
    let query = format!(
        "recording:\"{}\" AND artist:\"{}\"",
        escape(title),
        escape(artist)
    );
    Some(format!(
        "recording/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&query)
    ))
}

/// The relative URL `FindStudioAlbumAsync` asks and the title its answer is read against, or
/// None when there is nothing to ask.
pub fn studio_album_search(artist: &str, title: &str) -> Option<(String, String)> {
    let plain_title = SongIdentity::strip_features(title);
    // Words, not a phrase: "They Dont Care About Us" has to find "They Don't Care About Us".
    // char.IsLetterOrDigit looks at one UTF-16 unit, so a character outside the BMP is two
    // spaces.
    let mut words = String::with_capacity(plain_title.len());
    for c in plain_title.chars() {
        if dotnet::is_letter_utf16(c) || dotnet::is_digit_utf16(c) {
            words.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                words.push(' ');
            }
        }
    }
    let words = words.trim();
    if dotnet::is_blank(artist) || words.is_empty() {
        return None;
    }
    let query = format!(
        "recording:({words}) AND artist:\"{}\" AND primarytype:album AND status:official",
        escape(artist)
    );
    Some((
        format!(
            "recording/?query={}&fmt=json&limit=50",
            dotnet::escape_data_string(&query)
        ),
        plain_title,
    ))
}

/// The release group of the oldest official studio album a song appears on, or of a
/// soundtrack when no studio album has it. None when the answer names neither.
pub fn pick_studio_album(root: &Value, title: &str) -> ElementResult<Option<String>> {
    let Some(recordings @ Value::Array(_)) = try_get_property(root, "recordings")? else {
        return Ok(None);
    };
    let wanted = SongIdentity::key(title);
    let mut group_id: Option<String> = None;
    let mut group_date: Option<String> = None;
    let mut group_rank = i32::MAX;
    for recording in enumerate_array(recordings)? {
        let Some(recording_title) = try_get_property(recording, "title")? else {
            continue;
        };
        if SongIdentity::key(get_string(recording_title)?.unwrap_or("")) != wanted {
            continue;
        }
        let Some(releases) = try_get_property(recording, "releases")? else {
            continue;
        };
        for release in enumerate_array(releases)? {
            let Some(group) = try_get_property(release, "release-group")? else {
                continue;
            };
            let Some(kind) = try_get_property(group, "primary-type")? else {
                continue;
            };
            if get_string(kind)? != Some("Album") {
                continue;
            }
            let mut secondary: Vec<Option<&str>> = Vec::new();
            if let Some(s @ Value::Array(_)) = try_get_property(group, "secondary-types")? {
                for x in enumerate_array(s)? {
                    secondary.push(get_string(x)?);
                }
            }
            let rank = match secondary.as_slice() {
                [] => 0,
                [Some("Soundtrack")] => 1,
                _ => -1,
            };
            if rank < 0 || rank > group_rank {
                continue;
            }

            let date = match try_get_property(release, "date")? {
                Some(d) => get_string(d)?.filter(|d| !d.is_empty()).map(str::to_string),
                None => None,
            };
            let earlier = match (&date, &group_date) {
                (Some(date), Some(known)) => date.as_str() < known.as_str(),
                (Some(_), None) => true,
                (None, _) => false,
            };
            if rank < group_rank || group_id.is_none() || earlier {
                group_id = get_string(get_property(group, "id")?)?.map(str::to_string);
                group_date = date;
                group_rank = rank;
            }
        }
    }
    Ok(group_id)
}

/// The "isrcs" list of a recording lookup, each one normalised; invalid ones dropped.
pub fn parse_isrcs(root: &Value) -> ElementResult<Vec<String>> {
    let mut codes: Vec<String> = Vec::new();
    if let Some(isrcs @ Value::Array(_)) = try_get_property(root, "isrcs")? {
        for isrc in enumerate_array(isrcs)? {
            if let Value::String(text) = isrc
                && let Some(code) = SongIdentity::normalize_isrc(text)
                && !codes.contains(&code)
            {
                codes.push(code);
            }
        }
    }
    Ok(codes)
}

/// The one recording that is this song, this version, by this artist, at this length.
pub fn pick(root: &Value, artist: &str, title: &str, duration_seconds: i32) -> ElementResult<Option<String>> {
    let Some(recordings @ Value::Array(_)) = try_get_property(root, "recordings")? else {
        return Ok(None);
    };

    // A HashSet with OrdinalIgnoreCase: the first spelling seen is kept.
    let mut ids: Vec<String> = Vec::new();
    for recording in enumerate_array(recordings)? {
        if let Some(score @ Value::Number(_)) = try_get_property(recording, "score")?
            && get_int32(score)? < 90
        {
            continue;
        }
        if let Some(Value::Bool(true)) = try_get_property(recording, "video")? {
            continue;
        }
        let Some(length @ Value::Number(_)) = try_get_property(recording, "length")? else {
            continue;
        };
        if (get_double(length)? / 1000.0 - f64::from(duration_seconds)).abs() > 3.0 {
            continue;
        }

        let name = match try_get_property(recording, "title")? {
            Some(t) => get_string(t)?.unwrap_or(""),
            None => "",
        };
        let disambiguation = match try_get_property(recording, "disambiguation")? {
            Some(d) => get_string(d)?.unwrap_or(""),
            None => "",
        };
        // A live take or a remix says so in its disambiguation, not always in its title.
        let described = if disambiguation.is_empty() {
            name.to_string()
        } else {
            format!("{name} ({disambiguation})")
        };
        if !SongIdentity::same_title(title, &described, Some(&SongIdentity::strict_titles())).is_same() {
            continue;
        }

        let mut credits: Vec<String> = Vec::new();
        if let Some(credit @ Value::Array(_)) = try_get_property(recording, "artist-credit")? {
            for entry in enumerate_array(credit)? {
                let name = match try_get_property(entry, "name")? {
                    Some(n) => get_string(n)?.unwrap_or(""),
                    None => "",
                };
                if !name.is_empty() {
                    credits.push(name.to_string());
                }
            }
        }
        if !TrackMatchComparer::artist_matches(artist, &credits.join(" & "), &credits) {
            continue;
        }

        if let Some(id) = try_get_property(recording, "id")?
            && let Some(recording_id) = get_string(id)?.filter(|id| !id.is_empty())
            && !ids
                .iter()
                .any(|known| dotnet::eq_ignore_case(known, recording_id))
        {
            ids.push(recording_id.to_string());
        }
    }
    Ok(if ids.len() == 1 { ids.pop() } else { None })
}

#[cfg(test)]
#[path = "music_brainz_client_tests.rs"]
mod tests;
